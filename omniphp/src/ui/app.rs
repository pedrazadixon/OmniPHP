//! The main window: title bar, sidebar and the current page.
//!
//! The view renders from a [`Snapshot`]. Every action runs on a background
//! thread against the shared [`Engine`] and hands back a fresh snapshot, so the
//! window never blocks while Apache starts, stops or validates its config.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, Theme, ThemeMode, TitleBar,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    sidebar::{Sidebar, SidebarMenu, SidebarMenuItem},
    spinner::Spinner,
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::core::config::ThemeChoice;
use crate::core::downloads::{self, PhpRelease};
use crate::core::engine::{Engine, NewSiteRequest, ServiceInfo, Snapshot};
use crate::core::quickapp::{self, Template};
use crate::core::php::SITE_DIRECTIVES;
use crate::core::runtimes::PhpRuntime;
use crate::core::services::{ServiceId, ServiceStatus};
use crate::core::sites::{PUBLIC_DIRS, Site};

pub type SharedEngine = Arc<Mutex<Engine>>;

/// A poisoned lock only means an earlier action panicked; the engine's state
/// is still the best information available.
pub fn lock(engine: &SharedEngine) -> MutexGuard<'_, Engine> {
    engine.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Services,
    Sites,
    /// Settings of the site held in `AppView::site_page`.
    Site,
    NewSite,
    Settings,
}

/// Choices made on the New site page.
struct NewSitePage {
    name: Entity<InputState>,
    template: Template,
    /// `None` lets the app pick (the default PHP, or what the template needs).
    php: Option<String>,
    create_database: bool,
    _name_changed: Subscription,
}

/// Text fields of the Settings page.
struct SettingsPage {
    http_port: Entity<InputState>,
    https_port: Entity<InputState>,
    mysql_port: Entity<InputState>,
    smtp_port: Entity<InputState>,
    inbox_port: Entity<InputState>,
    redis_port: Entity<InputState>,
    pattern: Entity<InputState>,
}

/// Applies the light / dark / follow-Windows choice.
pub fn apply_theme(choice: ThemeChoice, window: Option<&mut Window>, cx: &mut App) {
    match choice {
        ThemeChoice::System => Theme::sync_system_appearance(window, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// State of the site settings page that is not part of the snapshot.
struct SitePage {
    name: String,
    hostname: Entity<InputState>,
    aliases: Entity<InputState>,
    /// Folders offered as document root, relative to the site ("" is the site itself).
    root_options: Vec<String>,
    /// One field per entry of [`SITE_DIRECTIVES`], in the same order.
    ini: Vec<Entity<InputState>>,
    /// Overrides beyond the common fields: setting name and its value field.
    other_ini: Vec<(String, Entity<InputState>)>,
    /// Search box over `directives`.
    ini_search: Entity<InputState>,
    /// Every setting of this site's PHP with its current value; `None` while
    /// the list is being fetched.
    directives: Option<Vec<(String, String)>>,
    /// Re-renders the page as the search text changes.
    _search_changed: Subscription,
}

pub struct AppView {
    engine: SharedEngine,
    snapshot: Snapshot,
    logs_dir: PathBuf,
    page: Page,
    site_page: Option<SitePage>,
    settings_page: Option<SettingsPage>,
    new_site_page: Option<NewSitePage>,
    /// The installer's current step while a site is being created.
    install_progress: Option<Arc<Mutex<String>>>,
    /// PHP builds offered for download, once the list has been asked for.
    php_releases: Option<Vec<PhpRelease>>,
    /// An action is in flight; controls that would start another are disabled.
    busy: bool,
    /// Quit was chosen: the window shows only a waiting message until the
    /// services have stopped and the app exits.
    quitting: bool,
    /// The application icon, shown in the title bar.
    logo: Arc<Image>,
    error: Option<SharedString>,
    /// A plain message for the user that is not an error.
    notice: Option<SharedString>,
    /// Filter box on the Sites page.
    site_search: Entity<InputState>,
    _site_search_changed: Subscription,
    _watch_sites: Task<()>,
}

impl AppView {
    pub fn new(engine: SharedEngine, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let site_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search sites"));
        let search_changed = cx.observe(&site_search, |_, _, cx| cx.notify());
        let (snapshot, logs_dir, startup_error, autostart) = {
            let mut engine = lock(&engine);
            (
                engine.snapshot(),
                engine.logs_dir(),
                engine.check_root().err().map(|error| format!("{error:#}")),
                engine.autostart(),
            )
        };

        // New or removed folders in the sites directory show up on their own.
        let watch_sites = cx.spawn({
            let engine = engine.clone();
            async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let engine = engine.clone();
                    let (error, snapshot) = cx
                        .background_executor()
                        .spawn(async move {
                            let mut engine = lock(&engine);
                            let error = engine.sync_sites().err().map(|error| format!("{error:#}"));
                            (error, engine.snapshot())
                        })
                        .await;
                    let alive = this.update(cx, |this, cx| {
                        if this.busy || (error.is_none() && this.snapshot == snapshot) {
                            return;
                        }
                        this.snapshot = snapshot;
                        if let Some(error) = error {
                            this.error = Some(error.into());
                        }
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }
        });

        let mut view = Self {
            engine,
            snapshot,
            logs_dir,
            page: Page::Services,
            site_page: None,
            settings_page: None,
            new_site_page: None,
            install_progress: None,
            php_releases: None,
            busy: false,
            quitting: false,
            logo: Arc::new(Image::from_bytes(
                ImageFormat::Png,
                include_bytes!("../../assets/icon-64.png").to_vec(),
            )),
            error: startup_error.map(Into::into),
            notice: None,
            site_search,
            _site_search_changed: search_changed,
            _watch_sites: watch_sites,
        };
        if autostart && view.error.is_none() {
            view.start_all(cx);
        }
        view
    }

    /// Runs `action` against the engine off the UI thread, then refreshes.
    fn run(
        &mut self,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut Engine) -> anyhow::Result<()> + Send + 'static,
    ) {
        if self.quitting {
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let (result, snapshot) = cx
                .background_executor()
                .spawn(async move {
                    let mut engine = lock(&engine);
                    let result = action(&mut engine).map_err(|error| format!("{error:#}"));
                    (result, engine.snapshot())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.snapshot = snapshot;
                this.error = result.err().map(Into::into);
                // The site's PHP version or extensions may just have changed,
                // and with them the settings it offers.
                if this.page == Page::Site {
                    this.load_directives(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apache_running(&self) -> bool {
        self.snapshot.is_running(ServiceId::Apache)
    }

    /// Also used by the tray menu.
    pub fn start_all(&mut self, cx: &mut Context<Self>) {
        self.run(cx, |engine| engine.start_all());
    }

    pub fn stop_all(&mut self, cx: &mut Context<Self>) {
        self.run(cx, |engine| engine.stop_all());
    }

    /// Quit from the tray menu: the window turns into a waiting screen while
    /// the services stop cleanly (MySQL flushes its data), and the app exits
    /// only once they have. If the app dies any other way, the job object set
    /// up in `core::proc` still takes the services down with it.
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        cx.notify();
        let engine = self.engine.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .spawn(async move {
                    let _ = lock(&engine).stop_all();
                })
                .await;
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    }

    fn render_quitting(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(Spinner::new().large())
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Stopping services…"))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("OmniPHP will close by itself as soon as it is safe. Please wait."),
            )
    }

    fn localhost_url(&self) -> String {
        match self.snapshot.http_port {
            80 => "http://localhost/".into(),
            port => format!("http://localhost:{port}/"),
        }
    }

    fn php_for(&self, site: &Site) -> Option<&PhpRuntime> {
        self.snapshot.phps.iter().find(|php| Some(&php.id) == site.php.as_ref())
    }

    // ---- shell -----------------------------------------------------------

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let nav = |label: &'static str, icon: IconName, page: Page, cx: &mut Context<Self>| {
            // The site settings page belongs to the Sites section.
            let current = if matches!(self.page, Page::Site | Page::NewSite) { Page::Sites } else { self.page };
            SidebarMenuItem::new(label)
                .icon(icon)
                .active(current == page)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.page = page;
                    this.notice = None;
                    cx.notify();
                }))
        };
        let default_php = self
            .snapshot
            .phps
            .iter()
            .find(|php| Some(&php.id) == self.snapshot.default_php.as_ref())
            .map_or("No PHP installed".to_string(), |php| format!("Default PHP {}", php.short_version()));

        Sidebar::new("nav")
            .collapsible(false)
            .child(SidebarMenu::new().children([
                nav("Services", IconName::HardDrive, Page::Services, cx),
                nav("Sites", IconName::Globe, Page::Sites, cx),
                SidebarMenuItem::new("Settings")
                    .icon(IconName::Settings)
                    .active(self.page == Page::Settings)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_settings(window, cx);
                    })),
            ]))
            .footer(
                v_flex()
                    .w_full()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(default_php)
                    .child(self.snapshot.root.display().to_string()),
            )
    }

    fn render_error(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let error = self.error.clone()?;
        let theme = cx.theme();
        Some(
            h_flex()
                .items_start()
                .justify_between()
                .gap_3()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.danger)
                .bg(theme.danger.opacity(0.08))
                .text_sm()
                .child(div().flex_1().min_w_0().child(error))
                .child(
                    Button::new("dismiss-error")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Dismiss")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_notice(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let notice = self.notice.clone()?;
        let theme = cx.theme();
        Some(
            h_flex()
                .items_start()
                .justify_between()
                .gap_3()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .text_sm()
                .child(div().flex_1().min_w_0().child(notice))
                .child(
                    Button::new("dismiss-notice")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Dismiss")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.notice = None;
                            cx.notify();
                        })),
                ),
        )
    }

    /// Opens the file a site's PHP errors are logged to, or says that there
    /// are none yet.
    fn open_php_errors(&mut self, site: &str, cx: &mut Context<Self>) {
        let log = self.logs_dir.join(format!("php-{site}.log"));
        let has_errors = log.metadata().is_ok_and(|file| file.len() > 0);
        if has_errors {
            self.notice = None;
            cx.open_with_system(&log);
        } else {
            self.notice = Some(
                format!(
                    "No PHP errors have been logged for {site} yet. When there are, they are written to {}.",
                    log.display()
                )
                .into(),
            );
        }
        cx.notify();
    }

    /// Shown while Windows still needs a change only an administrator can make.
    fn render_setup(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.snapshot.needs_system_setup() {
            return None;
        }
        let theme = cx.theme();
        let pending = self.snapshot.unregistered_hosts.len();
        let message = match (self.snapshot.ca_trusted, pending) {
            (false, 0) => "Windows does not trust OmniPHP's HTTPS certificates yet, so browsers will show a warning.".to_string(),
            (false, _) => "Windows needs two changes before your sites open in a browser: trusting OmniPHP's HTTPS certificates and registering the site addresses.".to_string(),
            (true, 1) => format!("{} is not registered with Windows yet, so it will not open in a browser.", self.snapshot.unregistered_hosts[0]),
            (true, n) => format!("{n} site addresses are not registered with Windows yet, so they will not open in a browser."),
        };
        Some(
            h_flex()
                .justify_between()
                .gap_3()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .text_sm()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(message)
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Windows will ask for administrator permission once."),
                        ),
                )
                .child(
                    Button::new("system-setup")
                        .primary()
                        .label("Set up")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.run(cx, |engine| engine.setup_system());
                        })),
                ),
        )
    }

    /// The Share section of a site's page: a temporary public address through
    /// a Cloudflare tunnel.
    fn render_share(&self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, success) = (theme.muted_foreground, theme.success);
        let site = name.to_string();

        if let Some(url) = self.snapshot.shares.get(name) {
            let (copy_url, open_url) = (url.clone(), url.clone());
            return h_flex()
                .justify_between()
                .gap_3()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .child(div().text_color(success).child(url.clone()))
                        .child(div().text_xs().text_color(muted).child(if self.apache_running() {
                            "Anyone with this link can open the site until you stop sharing or quit OmniPHP."
                        } else {
                            "Apache is stopped: the link shows an error until Apache runs again."
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("share-copy").label("Copy link").icon(IconName::Copy).on_click(
                            move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone())),
                        ))
                        .child(
                            Button::new("share-open")
                                .label("Open")
                                .icon(IconName::ExternalLink)
                                .on_click(move |_, _, cx| cx.open_url(&open_url)),
                        )
                        .child(Button::new("share-stop").label("Stop sharing").disabled(self.busy).on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let site = site.clone();
                                this.run(cx, move |engine| engine.unshare_site(&site));
                            }),
                        )),
                )
                .into_any_element();
        }

        let hint = if !self.snapshot.can_share {
            "cloudflared is not installed: put cloudflared.exe in bin\\cloudflared."
        } else if !self.apache_running() {
            "Start Apache first."
        } else {
            "Gives the site a temporary public address through Cloudflare. No account needed."
        };
        h_flex()
            .justify_between()
            .gap_3()
            .child(div().flex_1().min_w_0().text_sm().text_color(muted).child(hint))
            .child(
                Button::new("share-start")
                    .label("Start sharing")
                    .disabled(self.busy || !self.snapshot.can_share || !self.apache_running())
                    .loading(self.busy)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let site = site.clone();
                        this.run(cx, move |engine| engine.share_site(&site));
                    })),
            )
            .into_any_element()
    }

    // ---- new site page ---------------------------------------------------

    fn open_new_site(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("my-site"));
        let name_changed = cx.observe(&name, |_, _, cx| cx.notify());
        self.new_site_page = Some(NewSitePage {
            name,
            template: Template::Blank,
            php: None,
            create_database: self.snapshot.mysql_installed,
            _name_changed: name_changed,
        });
        self.page = Page::NewSite;
        cx.notify();
    }

    /// Creates the site in the background, showing each step, then opens its
    /// settings page.
    fn create_site(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &self.new_site_page else { return };
        let request = NewSiteRequest {
            template: page.template,
            name: page.name.read(cx).value().trim().to_string(),
            php: page.php.clone(),
            create_database: page.create_database && page.template != Template::Blank,
        };
        let progress = Arc::new(Mutex::new(String::from("Preparing…")));
        self.install_progress = Some(progress.clone());
        self.busy = true;
        self.error = None;
        cx.notify();

        // Repaint while the installer reports its steps.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let installing = this.update(cx, |this, cx| {
                    cx.notify();
                    this.install_progress.is_some()
                });
                if !matches!(installing, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (result, snapshot) = cx
                .background_executor()
                .spawn(async move {
                    let report = |step: &str| {
                        *progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = step.to_string();
                    };
                    // The engine is only held for the quick first and last
                    // steps, so the rest of the app stays usable meanwhile.
                    let result = (|| {
                        let job = lock(&engine).prepare_site(&request)?;
                        if let Err(error) = quickapp::install(&job, &report) {
                            lock(&engine).discard_site(&job);
                            return Err(error);
                        }
                        report("Finishing…");
                        lock(&engine).finish_site(&job)?;
                        Ok(job.name)
                    })();
                    let snapshot = lock(&engine).snapshot();
                    (result.map_err(|error: anyhow::Error| format!("{error:#}")), snapshot)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this.install_progress = None;
                this.snapshot = snapshot;
                match result {
                    Ok(name) => {
                        this.new_site_page = None;
                        this.open_site(name, window, cx);
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_new_site(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(page) = &self.new_site_page else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let template = page.template;
        let typed = page.name.read(cx).value().trim().to_string();
        let name_error = (!typed.is_empty()).then(|| quickapp::validate_name(&typed).err()).flatten();
        let address = if typed.is_empty() || name_error.is_some() {
            String::new()
        } else {
            format!("https://{}", self.snapshot.config.sites.hostname_pattern.replace("{name}", &typed))
        };

        let header = v_flex()
            .items_start()
            .child(
                Button::new("new-site-back")
                    .link()
                    .small()
                    .label("Sites")
                    .icon(IconName::ArrowLeft)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.page = Page::Sites;
                        cx.notify();
                    })),
            )
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("New site"))
            .child(div().text_sm().text_color(muted).child(format!(
                "Creates a folder in {} and installs the app into it.",
                self.snapshot.www.display()
            )));

        let templates = h_flex().flex_wrap().gap_2().children(Template::ALL.into_iter().enumerate().map(
            |(ix, option)| {
                Button::new(("new-site-template", ix))
                    .label(option.label())
                    .when(option == template, |button| button.primary())
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(page) = &mut this.new_site_page {
                            page.template = option;
                            // A template tied to older PHP picks its own version.
                            if option.max_php().is_some() {
                                page.php = None;
                            }
                            cx.notify();
                        }
                    }))
            },
        ));

        let default_label = match template.max_php() {
            Some(max) => format!("Newest up to PHP {max}"),
            None => "Default".to_string(),
        };
        let php_choices = h_flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("new-site-php-default")
                    .label(default_label)
                    .when(page.php.is_none(), |button| button.primary())
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(page) = &mut this.new_site_page {
                            page.php = None;
                            cx.notify();
                        }
                    })),
            )
            .children(self.snapshot.phps.iter().enumerate().map(|(ix, runtime)| {
                let id = runtime.id.clone();
                Button::new(("new-site-php", ix))
                    .label(format!("PHP {}", runtime.version))
                    .when(page.php.as_ref() == Some(&runtime.id), |button| button.primary())
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(page) = &mut this.new_site_page {
                            page.php = Some(id.clone());
                            cx.notify();
                        }
                    }))
            }));

        let database_label = if typed.is_empty() || name_error.is_some() {
            "Create a MySQL database for the site".to_string()
        } else {
            format!("Create the MySQL database {}", quickapp::database_name(&typed))
        };
        let wants_database = template != Template::Blank && self.snapshot.mysql_installed;
        let database = Checkbox::new("new-site-database")
            .label(database_label)
            .checked(page.create_database && wants_database)
            .disabled(self.busy || !wants_database)
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                if let Some(page) = &mut this.new_site_page {
                    page.create_database = *checked;
                    cx.notify();
                }
            }));

        let progress = self
            .install_progress
            .as_ref()
            .map(|progress| progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone());
        let can_create = !self.busy && !typed.is_empty() && name_error.is_none();

        v_flex()
            .gap_5()
            .child(header)
            .child(
                section("Start from", cx)
                    .child(templates)
                    .child(div().text_xs().text_color(muted).child(template.note())),
            )
            .child(
                section("Folder name", cx)
                    .child(div().w(px(320.)).child(Input::new(&page.name)))
                    .child(div().text_xs().text_color(muted).child(match &name_error {
                        Some(error) => format!("{error}"),
                        None if address.is_empty() => "The name also becomes the site's address.".to_string(),
                        None => format!("The site will open at {address}"),
                    })),
            )
            .child(section("PHP version", cx).child(php_choices))
            .child(section("Database", cx).child(database))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Button::new("new-site-create")
                            .primary()
                            .label("Create site")
                            .disabled(!can_create)
                            .loading(self.install_progress.is_some())
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.create_site(window, cx);
                            })),
                    )
                    .children(progress.map(|step| div().text_sm().text_color(muted).child(step))),
            )
            .into_any_element()
    }

    // ---- settings page ---------------------------------------------------

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = &self.snapshot.config;
        let mut input = |value: String| cx.new(|cx| InputState::new(window, cx).default_value(value));
        self.settings_page = Some(SettingsPage {
            http_port: input(config.apache.http_port.to_string()),
            https_port: input(config.apache.https_port.to_string()),
            mysql_port: input(config.mysql.port.to_string()),
            smtp_port: input(config.mailpit.smtp_port.to_string()),
            inbox_port: input(config.mailpit.ui_port.to_string()),
            redis_port: input(config.redis.port.to_string()),
            pattern: input(config.sites.hostname_pattern.clone()),
        });
        self.page = Page::Settings;
        cx.notify();
    }

    /// Reads the text fields and saves them; running services restart on
    /// their new ports.
    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &self.settings_page else { return };
        let port = |label: &str, input: &Entity<InputState>| -> Result<u16, String> {
            let text = input.read(cx).value();
            match text.trim().parse::<u16>() {
                Ok(port) if port > 0 => Ok(port),
                _ => Err(format!("{label}: \"{}\" is not a port number (1 to 65535).", text.trim())),
            }
        };
        let parsed = (|| {
            Ok::<_, String>((
                port("Apache HTTP port", &page.http_port)?,
                port("Apache HTTPS port", &page.https_port)?,
                port("MySQL port", &page.mysql_port)?,
                port("Mailpit SMTP port", &page.smtp_port)?,
                port("Mailpit inbox port", &page.inbox_port)?,
                port("Redis port", &page.redis_port)?,
            ))
        })();
        let pattern = page.pattern.read(cx).value().trim().to_lowercase();
        match parsed {
            Ok((http, https, mysql, smtp, inbox, redis)) => self.run(cx, move |engine| {
                engine.update_config(|config| {
                    config.apache.http_port = http;
                    config.apache.https_port = https;
                    config.mysql.port = mysql;
                    config.mailpit.smtp_port = smtp;
                    config.mailpit.ui_port = inbox;
                    config.redis.port = redis;
                    config.sites.hostname_pattern = pattern;
                })
            }),
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
            }
        }
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(page) = &self.settings_page else {
            return div().into_any_element();
        };
        let general = &self.snapshot.config.general;

        let startup = Switch::new("setting-startup")
            .accessibility_label("Run OmniPHP when Windows starts")
            .checked(self.snapshot.run_at_startup)
            .disabled(self.busy)
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                let on = *checked;
                this.run(cx, move |engine| engine.set_run_at_startup(on));
            }));
        let minimized = Switch::new("setting-minimized")
            .accessibility_label("Start minimized to the tray")
            .checked(general.start_minimized)
            .disabled(self.busy)
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                let on = *checked;
                this.run(cx, move |engine| {
                    engine.update_config(|config| config.general.start_minimized = on)
                });
            }));
        let autostart = Switch::new("setting-autostart")
            .accessibility_label("Start services automatically")
            .checked(general.autostart_services)
            .disabled(self.busy)
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                let on = *checked;
                this.run(cx, move |engine| {
                    engine.update_config(|config| config.general.autostart_services = on)
                });
            }));

        let current_theme = general.theme;
        let themes = h_flex().gap_2().children(
            [("Follow Windows", ThemeChoice::System), ("Light", ThemeChoice::Light), ("Dark", ThemeChoice::Dark)]
                .into_iter()
                .enumerate()
                .map(|(ix, (label, choice))| {
                    Button::new(("setting-theme", ix))
                        .small()
                        .label(label)
                        .when(choice == current_theme, |button| button.primary())
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            apply_theme(choice, Some(window), cx);
                            this.run(cx, move |engine| {
                                engine.update_config(|config| config.general.theme = choice)
                            });
                        }))
                }),
        );

        let port_field = |label: &'static str, state: &Entity<InputState>, cx: &App| {
            field(label, cx).w(px(150.)).child(Input::new(state))
        };

        v_flex()
            .gap_5()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Settings"))
            .child(
                card(cx)
                    .child(setting_row(
                        "Run OmniPHP when Windows starts",
                        "Adds OmniPHP to your startup apps.",
                        startup,
                        cx,
                    ))
                    .child(setting_row(
                        "Start minimized to the tray",
                        "Opens without showing this window.",
                        minimized,
                        cx,
                    ))
                    .child(setting_row(
                        "Start services automatically",
                        "Starts every enabled service when OmniPHP opens.",
                        autostart,
                        cx,
                    ))
                    .child(setting_row("Theme", "", themes, cx)),
            )
            .child(
                section("Ports and addresses", cx)
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_3()
                            .child(port_field("Apache HTTP", &page.http_port, cx))
                            .child(port_field("Apache HTTPS", &page.https_port, cx))
                            .child(port_field("MySQL", &page.mysql_port, cx))
                            .child(port_field("Mailpit SMTP", &page.smtp_port, cx))
                            .child(port_field("Mailpit inbox", &page.inbox_port, cx))
                            .child(port_field("Redis", &page.redis_port, cx)),
                    )
                    .child(
                        h_flex()
                            .items_end()
                            .justify_between()
                            .gap_3()
                            .child(
                                field("Address pattern for new sites", cx)
                                    .w(px(315.))
                                    .child(Input::new(&page.pattern)),
                            )
                            .child(
                                Button::new("settings-save")
                                    .primary()
                                    .label("Save and restart services")
                                    .disabled(self.busy)
                                    .loading(self.busy)
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.save_settings(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    // ---- site settings page ----------------------------------------------

    fn open_site(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(site) = self.snapshot.sites.iter().find(|site| site.name == name) else {
            return;
        };
        // Offer the site folder, the usual public folders, and whatever is in use now.
        let mut root_options = vec![String::new()];
        root_options.extend(
            PUBLIC_DIRS.into_iter().filter(|sub| site.dir.join(sub).is_dir()).map(str::to_string),
        );
        let current = relative_root(site);
        if !root_options.contains(&current) {
            root_options.push(current);
        }
        let hostname = site.hostname.clone();
        let aliases = site.aliases.join(" ");
        let saved_ini = site.ini.clone();
        let ini = SITE_DIRECTIVES
            .iter()
            .map(|directive| {
                let value = saved_ini.get(directive.name).cloned().unwrap_or_default();
                cx.new(|cx| {
                    InputState::new(window, cx).placeholder(directive.default).default_value(value)
                })
            })
            .collect();
        let other_ini = saved_ini
            .iter()
            .filter(|(name, _)| !SITE_DIRECTIVES.iter().any(|common| common.name == name.as_str()))
            .map(|(name, value)| {
                let value = value.clone();
                (name.clone(), cx.new(|cx| InputState::new(window, cx).default_value(value)))
            })
            .collect();
        let ini_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search settings, for example opcache or session")
        });
        let search_changed = cx.observe(&ini_search, |_, _, cx| cx.notify());
        self.site_page = Some(SitePage {
            other_ini,
            ini_search,
            directives: None,
            _search_changed: search_changed,
            ini,
            name,
            hostname: cx.new(|cx| InputState::new(window, cx).default_value(hostname)),
            aliases: cx.new(|cx| {
                InputState::new(window, cx).placeholder("None").default_value(aliases)
            }),
            root_options,
        });
        self.page = Page::Site;
        self.load_directives(cx);
        cx.notify();
    }

    /// Asks the site's PHP for its full list of settings, in the background.
    fn load_directives(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.site_page.as_ref().map(|page| page.name.clone()) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn({
                    let name = name.clone();
                    async move { lock(&engine).php_directives(&name) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(page) = &mut this.site_page
                    && page.name == name
                {
                    // An empty list still lets settings be added by typing their name.
                    page.directives = Some(listed.unwrap_or_default());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The "Other settings" part of a site's PHP settings: the overrides the
    /// site has, and a search over everything its PHP supports to add more.
    fn render_other_ini(&self, page: &SitePage, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let query = page.ini_search.read(cx).value().trim().to_lowercase();
        let taken = |name: &str| {
            SITE_DIRECTIVES.iter().any(|common| common.name == name)
                || page.other_ini.iter().any(|(existing, _)| existing == name)
        };

        let overrides = page.other_ini.iter().enumerate().map(|(ix, (name, state))| {
            let remove = name.clone();
            h_flex()
                .gap_3()
                .child(div().w(px(280.)).flex_shrink_0().text_sm().child(name.clone()))
                .child(div().flex_1().min_w_0().child(Input::new(state)))
                .child(Button::new(("ini-remove", ix)).small().label("Remove").on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        if let Some(page) = &mut this.site_page {
                            page.other_ini.retain(|(name, _)| *name != remove);
                            cx.notify();
                        }
                    },
                )))
                .into_any_element()
        });
        let overrides: Vec<AnyElement> = overrides.collect();

        let add_button = |ix: usize, name: String, value: String, cx: &mut Context<Self>| {
            Button::new(("ini-add", ix)).small().label("Add").on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| {
                    let Some(page) = &mut this.site_page else { return };
                    let value = value.clone();
                    let state = cx.new(|cx| InputState::new(window, cx).default_value(value));
                    page.other_ini.push((name.clone(), state));
                    page.ini_search.update(cx, |search, cx| search.set_value("", window, cx));
                    cx.notify();
                },
            ))
        };

        let matches: Vec<&(String, String)> = match &page.directives {
            Some(all) if !query.is_empty() => all
                .iter()
                .filter(|(name, _)| name.to_lowercase().contains(&query) && !taken(name))
                .take(8)
                .collect(),
            _ => Vec::new(),
        };
        let total_matches = page.directives.as_ref().map_or(0, |all| {
            all.iter().filter(|(name, _)| name.to_lowercase().contains(&query) && !taken(name)).count()
        });
        let results = matches.iter().enumerate().map(|(ix, (name, value))| {
            let shown = if value.is_empty() { "(empty)".to_string() } else { value.clone() };
            h_flex()
                .gap_3()
                .py_1()
                .border_t_1()
                .border_color(border)
                .text_sm()
                .child(div().w(px(280.)).flex_shrink_0().child(name.clone()))
                .child(div().flex_1().min_w_0().truncate().text_color(muted).child(shown))
                .child(add_button(ix, name.clone(), value.clone(), cx))
                .into_any_element()
        });
        let results: Vec<AnyElement> = results.collect();

        // A name PHP did not list (an extension not enabled yet) can still be added.
        let exact = page.directives.as_ref().is_some_and(|all| all.iter().any(|(name, _)| *name == query));
        let typed_name_ok = !query.is_empty()
            && query.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'));
        let custom = (page.directives.is_some() && typed_name_ok && !exact && total_matches == 0 && !taken(&query)).then(|| {
            h_flex()
                .gap_3()
                .py_1()
                .border_t_1()
                .border_color(border)
                .text_sm()
                .child(div().flex_1().min_w_0().text_color(muted).child(format!(
                    "\"{query}\" is not a setting of this PHP with the extensions enabled now. Add it anyway?"
                )))
                .child(add_button(usize::MAX, query.clone(), String::new(), cx))
        });

        let status = match &page.directives {
            None => "Loading the list of settings…".to_string(),
            Some(all) if query.is_empty() => format!("{} settings available.", all.len()),
            Some(_) if total_matches > matches.len() => {
                format!("Showing {} of {total_matches} matches. Type more to narrow it down.", matches.len())
            }
            Some(_) => format!("{total_matches} matches."),
        };

        v_flex()
            .gap_2()
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Other settings"))
            .children(overrides)
            .child(Input::new(&page.ini_search))
            .child(div().text_xs().text_color(muted).child(status))
            .children(results)
            .children(custom)
    }

    fn render_site(&self, cx: &mut Context<Self>) -> AnyElement {
        let site = self
            .site_page
            .as_ref()
            .and_then(|page| self.snapshot.sites.iter().find(|site| site.name == page.name).map(|site| (page, site)));
        let Some((page, site)) = site else {
            // The folder was deleted while its settings were open.
            return self.render_sites(cx).into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let url = self.snapshot.site_url(site);
        let name = page.name.clone();
        let current_root = relative_root(site);
        let php = self.php_for(site);

        let header = h_flex()
            .justify_between()
            .gap_3()
            .child(
                v_flex()
                    .items_start()
                    .child(
                        Button::new("back-to-sites")
                            .link()
                            .small()
                            .label("Sites")
                            .icon(IconName::ArrowLeft)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.page = Page::Sites;
                                cx.notify();
                            })),
                    )
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(site.name.clone()))
                    .child(div().text_sm().text_color(muted).child(url.clone())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("site-open")
                            .label("Open in browser")
                            .icon(IconName::ExternalLink)
                            .disabled(!self.apache_running())
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(Button::new("site-folder").label("Folder").icon(IconName::FolderOpen).on_click({
                        let dir = site.dir.clone();
                        move |_, _, cx| cx.open_with_system(&dir)
                    }))
                    .child(Button::new("site-php-errors").label("PHP errors").icon(IconName::TriangleAlert).on_click(
                        cx.listener({
                            let name = name.clone();
                            move |this, _: &ClickEvent, _, cx| this.open_php_errors(&name, cx)
                        }),
                    ))
                    .child(Button::new("site-terminal").label("Terminal").icon(IconName::SquareTerminal).on_click(
                        cx.listener({
                            let name = name.clone();
                            move |this, _: &ClickEvent, _, cx| {
                                let name = name.clone();
                                this.run(cx, move |engine| engine.open_terminal(Some(&name)));
                            }
                        }),
                    ))
                    .child(
                        Button::new("site-reset")
                            .label("Reset to defaults")
                            .disabled(self.busy)
                            .on_click(cx.listener({
                                let name = name.clone();
                                move |this, _: &ClickEvent, window, cx| {
                                    let name = name.clone();
                                    this.run(cx, {
                                        let name = name.clone();
                                        move |engine| engine.update_site(&name, |settings| *settings = Default::default())
                                    });
                                    // Rebuild the inputs so they show the defaults again.
                                    this.site_page = None;
                                    this.open_site_when_idle(name, window, cx);
                                }
                            })),
                    ),
            );

        let php_choices = h_flex().flex_wrap().gap_2().children(self.snapshot.phps.iter().enumerate().map(|(ix, runtime)| {
            let selected = site.php.as_ref() == Some(&runtime.id);
            let (name, id) = (name.clone(), runtime.id.clone());
            Button::new(("site-php", ix))
                .label(format!("PHP {}", runtime.version))
                .when(selected, |button| button.primary())
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let (name, id) = (name.clone(), id.clone());
                    this.run(cx, move |engine| engine.update_site(&name, |settings| settings.php = Some(id)));
                }))
        }));

        let root_choices = h_flex().flex_wrap().gap_2().children(page.root_options.iter().enumerate().map(|(ix, option)| {
            let selected = *option == current_root;
            let label = if option.is_empty() { "/ (site folder)".to_string() } else { format!("/{option}") };
            let (name, option) = (name.clone(), option.clone());
            Button::new(("site-root", ix))
                .label(label)
                .when(selected, |button| button.primary())
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let (name, option) = (name.clone(), option.clone());
                    this.run(cx, move |engine| {
                        engine.update_site(&name, |settings| settings.document_root = Some(option))
                    });
                }))
        }));

        let addresses = h_flex()
            .items_end()
            .gap_3()
            .child(field("Address", cx).w(px(260.)).child(Input::new(&page.hostname)))
            .child(
                field("Other addresses (separate with spaces)", cx)
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&page.aliases)),
            )
            .child(
                Button::new("site-save-addresses")
                    .label("Save addresses")
                    .disabled(self.busy)
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |this, _: &ClickEvent, _, cx| {
                            let Some(page) = &this.site_page else { return };
                            let hostname = page.hostname.read(cx).value().trim().to_lowercase();
                            let aliases: Vec<String> = page
                                .aliases
                                .read(cx)
                                .value()
                                .split([' ', ',', ';'])
                                .map(|alias| alias.trim().to_lowercase())
                                .filter(|alias| !alias.is_empty())
                                .collect();
                            let name = name.clone();
                            this.run(cx, move |engine| {
                                engine.update_site(&name, |settings| {
                                    settings.hostname = (!hostname.is_empty()).then_some(hostname);
                                    settings.aliases = aliases;
                                })
                            });
                        }
                    })),
            );

        let extensions = match php {
            Some(runtime) => h_flex()
                .flex_wrap()
                .gap_y_2()
                .children(runtime.extensions.iter().enumerate().map(|(ix, extension)| {
                    let (name, ext) = (name.clone(), extension.clone());
                    div().w(px(170.)).child(
                        Checkbox::new(("site-ext", ix))
                            .label(extension.clone())
                            .checked(site.extensions.contains(extension))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                let (name, ext, checked) = (name.clone(), ext.clone(), *checked);
                                this.run(cx, move |engine| engine.set_site_extension(&name, &ext, checked));
                            })),
                    )
                }))
                .into_any_element(),
            None => div().text_sm().text_color(muted).child("No PHP is installed.").into_any_element(),
        };
        let php_settings = v_flex()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Leave a field empty to use the value shown in grey."),
            )
            .child(h_flex().flex_wrap().gap_3().children(
                SITE_DIRECTIVES.iter().zip(&page.ini).map(|(directive, state)| {
                    field(directive.label, cx).w(px(180.)).child(Input::new(state))
                }),
            ))
            .child(self.render_other_ini(page, cx))
            .child(
                h_flex().justify_end().child(
                    Button::new("site-save-ini")
                        .label("Save PHP settings")
                        .disabled(self.busy || php.is_none())
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _: &ClickEvent, _, cx| {
                                let Some(page) = &this.site_page else { return };
                                // The page shows every override the site has, so
                                // what is on screen becomes the whole list. An
                                // emptied field means "back to the default".
                                let common = SITE_DIRECTIVES
                                    .iter()
                                    .zip(&page.ini)
                                    .map(|(directive, state)| (directive.name.to_string(), state));
                                let others = page.other_ini.iter().map(|(name, state)| (name.clone(), state));
                                let ini: std::collections::BTreeMap<String, String> = common
                                    .chain(others)
                                    .map(|(name, state)| (name, state.read(cx).value().trim().to_string()))
                                    .filter(|(_, value)| !value.is_empty())
                                    .collect();
                                let name = name.clone();
                                this.run(cx, move |engine| {
                                    engine.update_site(&name, |settings| settings.ini = ini)
                                });
                            }
                        })),
                ),
            );

        let extension_title = match php {
            Some(runtime) => format!(
                "PHP extensions ({} of {} enabled)",
                site.extensions.len(),
                runtime.extensions.len()
            ),
            None => "PHP extensions".to_string(),
        };

        v_flex()
            .gap_5()
            .child(header)
            .child(section("PHP version", cx).child(php_choices))
            .child(section("Document root", cx).child(root_choices))
            .child(section("Addresses", cx).child(addresses))
            .child(section("Share on the internet", cx).child(self.render_share(&name, cx)))
            .child(section("PHP settings", cx).child(php_settings))
            .child(section(extension_title, cx).child(extensions))
            .into_any_element()
    }

    /// Reopens a site's settings once the action in flight has finished, so
    /// the inputs are rebuilt from the new values.
    fn open_site_when_idle(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let done = this.update_in(cx, |this, window, cx| {
                    if this.busy {
                        return false;
                    }
                    this.open_site(name.clone(), window, cx);
                    true
                });
                if !matches!(done, Ok(false)) {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- services page ---------------------------------------------------

    fn render_services(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.snapshot.running_count() > 0;
        let summary = format!(
            "{} of {} running",
            self.snapshot.running_count(),
            self.snapshot.services.len()
        );

        let header = page_header("Services", summary, cx)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("toggle-all")
                            .primary()
                            .label(if running { "Stop all" } else { "Start all" })
                            .disabled(self.busy)
                            .loading(self.busy)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if running {
                                    this.stop_all(cx);
                                } else {
                                    this.start_all(cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("open-web")
                            .label("Web")
                            .icon(IconName::Globe)
                            .disabled(!self.apache_running())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                cx.open_url(&this.localhost_url());
                            })),
                    )
                    .child(
                        Button::new("open-database")
                            .label("Database")
                            .disabled(self.busy || !self.apache_running() || !self.snapshot.mysql_installed)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.run(cx, |engine| engine.open_database());
                            })),
                    )
                    .child(
                        Button::new("open-terminal")
                            .label("Terminal")
                            .icon(IconName::SquareTerminal)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.run(cx, |engine| engine.open_terminal(None));
                            })),
                    )
                    .child(
                        Button::new("edit-hosts")
                            .label("Hosts")
                            .icon(IconName::FileText)
                            .tooltip("Open the Windows hosts file as administrator")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.run(cx, |engine| engine.edit_hosts());
                            })),
                    )
                    .child(
                        Button::new("open-root")
                            .label("Root")
                            .icon(IconName::FolderOpen)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                cx.open_with_system(&this.snapshot.root);
                            })),
                    ),
            );

        v_flex()
            .gap_5()
            .child(header)
            .child(
                card(cx).child(table_head(["Service", "Ports", "Status", ""], cx)).children(
                    self.snapshot
                        .services
                        .iter()
                        .map(|service| self.render_service_row(service, cx).into_any_element()),
                ),
            )
            .child(
                card(cx)
                    .child(table_head(["PHP version (FastCGI)", "Build", "Used by", ""], cx))
                    .children(
                        self.snapshot
                            .phps
                            .iter()
                            .enumerate()
                            .map(|(ix, php)| self.render_php_row(ix, php, cx).into_any_element()),
                    )
                    .when(self.snapshot.phps.is_empty(), |card| {
                        card.child(empty_row("No PHP found. Unzip a PHP build into bin\\php.", cx))
                    })
                    .child(self.render_php_downloads(cx)),
            )
    }

    fn render_service_row(&self, service: &ServiceInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let id = service.id;
        let running = service.status != ServiceStatus::Stopped;
        let theme = cx.theme();
        let (status, status_color) = if running {
            ("Running", theme.success)
        } else {
            ("Stopped", theme.muted_foreground)
        };
        let muted = theme.muted_foreground;
        let (key, log_file) = match id {
            ServiceId::Apache => ("apache", "apache-error.log"),
            ServiceId::Mysql => ("mysql", "mysql-error.log"),
            ServiceId::Mailpit => ("mailpit", "mailpit.log"),
            ServiceId::Redis => ("redis", "redis.log"),
        };
        let log = self.logs_dir.join(log_file);

        // The one action that only makes sense for this service.
        let special = match id {
            ServiceId::Apache => Some(
                Button::new("apache-reload")
                    .small()
                    .label("Reload")
                    .disabled(self.busy || !running)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.run(cx, |engine| engine.reload_apache());
                    })),
            ),
            ServiceId::Mailpit => {
                let inbox = format!("http://localhost:{}/", self.snapshot.config.mailpit.ui_port);
                Some(
                    Button::new("mailpit-inbox")
                        .small()
                        .label("Inbox")
                        .disabled(!running)
                        .on_click(move |_, _, cx| cx.open_url(&inbox)),
                )
            }
            ServiceId::Mysql | ServiceId::Redis => None,
        };

        row(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(
                        Switch::new(SharedString::from(format!("{key}-switch")))
                            .accessibility_label(id.name())
                            .checked(running)
                            .disabled(self.busy || !service.installed)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                let on = *checked;
                                this.run(cx, move |engine| engine.set_service(id, on));
                            })),
                    )
                    .child(
                        v_flex()
                            .child(div().font_weight(FontWeight::MEDIUM).child(id.name()))
                            .child(div().text_xs().text_color(muted).child(service.detail.clone())),
                    ),
            )
            .child(cell().child(service.ports.clone()))
            .child(
                cell().child(
                    h_flex()
                        .gap_1p5()
                        .text_color(status_color)
                        .child(div().size_2().rounded_full().bg(status_color))
                        .child(status),
                ),
            )
            .child(
                actions().children(special).child(
                    Button::new(SharedString::from(format!("{key}-log")))
                        .small()
                        .label("Log")
                        .on_click(move |_, _, cx| cx.open_with_system(&log)),
                ),
            )
    }

    /// Fetches the list of PHP builds that can be downloaded.
    fn load_php_releases(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let listed = cx.background_executor().spawn(async { downloads::available_php() }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match listed {
                    Ok(releases) => this.php_releases = Some(releases),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Downloads a PHP build into `bin\php`, showing each step.
    fn install_php(&mut self, release: PhpRelease, cx: &mut Context<Self>) {
        let progress = Arc::new(Mutex::new(format!("Downloading PHP {}…", release.version)));
        self.install_progress = Some(progress.clone());
        self.busy = true;
        self.error = None;
        cx.notify();

        // Repaint while the download reports its steps.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let installing = this.update(cx, |this, cx| {
                    cx.notify();
                    this.install_progress.is_some()
                });
                if !matches!(installing, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let (result, snapshot) = cx
                .background_executor()
                .spawn(async move {
                    let report = |step: &str| {
                        *progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = step.to_string();
                    };
                    // The engine is not held during the download.
                    let paths = lock(&engine).paths();
                    let result = downloads::install_php(&release, &paths, &report);
                    let mut engine = lock(&engine);
                    engine.rescan_runtimes();
                    (result.map(|_| ()).map_err(|error| format!("{error:#}")), engine.snapshot())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.install_progress = None;
                this.snapshot = snapshot;
                this.error = result.err().map(Into::into);
                cx.notify();
            });
        })
        .detach();
    }

    /// Bottom of the PHP card: a button to list downloadable builds, then the
    /// builds that are not installed yet.
    fn render_php_downloads(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let progress = (self.page == Page::Services)
            .then(|| self.install_progress.as_ref())
            .flatten()
            .map(|progress| progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone());

        let Some(releases) = &self.php_releases else {
            return row(cx)
                .justify_between()
                .child(div().text_color(muted).child("More versions can be downloaded from windows.php.net."))
                .child(
                    Button::new("php-list-releases")
                        .small()
                        .label("Add PHP version")
                        .icon(IconName::Plus)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.load_php_releases(cx))),
                )
                .into_any_element();
        };

        let missing: Vec<&PhpRelease> = releases
            .iter()
            .filter(|release| !self.snapshot.phps.iter().any(|php| php.version == release.version))
            .collect();
        let rows: Vec<AnyElement> = missing
            .iter()
            .enumerate()
            .map(|(ix, release)| {
                let release = (*release).clone();
                row(cx)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(div().font_weight(FontWeight::MEDIUM).child(format!("PHP {}", release.version)))
                            .child(div().text_color(muted).child(format!("Non-thread-safe · {}", release.size))),
                    )
                    .child(
                        actions().child(
                            Button::new(("php-install", ix))
                                .small()
                                .label("Download")
                                .disabled(self.busy)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.install_php(release.clone(), cx);
                                })),
                        ),
                    )
                    .into_any_element()
            })
            .collect();
        let status = match (progress, missing.is_empty()) {
            (Some(step), _) => step,
            (None, true) => "Every current PHP release is already installed.".to_string(),
            (None, false) => "Available from windows.php.net:".to_string(),
        };
        v_flex()
            .child(row(cx).text_color(muted).child(status))
            .children(rows)
            .into_any_element()
    }

    fn render_php_row(&self, ix: usize, php: &PhpRuntime, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let is_default = self.snapshot.default_php.as_ref() == Some(&php.id);
        let users = self.snapshot.sites.iter().filter(|site| site.php.as_ref() == Some(&php.id)).count();
        let used_by = match users {
            0 => "No sites".to_string(),
            1 => "1 site".to_string(),
            n => format!("{n} sites"),
        };
        let id = php.id.clone();

        row(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(div().font_weight(FontWeight::MEDIUM).child(format!("PHP {}", php.version)))
                    .when(is_default, |this| {
                        this.child(
                            div()
                                .px_2()
                                .rounded_full()
                                .bg(theme.primary)
                                .text_color(theme.primary_foreground)
                                .text_xs()
                                .child("Default"),
                        )
                    }),
            )
            .child(cell().text_color(muted).child(if php.thread_safe {
                "Thread-safe"
            } else {
                "Non-thread-safe"
            }))
            .child(cell().child(used_by))
            .child(
                actions().child(
                    Button::new(("php-default", ix))
                        .small()
                        .label("Set default")
                        .disabled(self.busy || is_default)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let id = id.clone();
                            this.run(cx, move |engine| engine.set_default_php(&id));
                        })),
                ),
            )
    }

    // ---- sites page ------------------------------------------------------

    fn render_sites(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.snapshot.sites.len();
        // The search matches the folder name and the site's addresses.
        let query = self.site_search.read(cx).value().trim().to_lowercase();
        let shown: Vec<(usize, &Site)> = self
            .snapshot
            .sites
            .iter()
            .enumerate()
            .filter(|(_, site)| {
                query.is_empty()
                    || site.name.to_lowercase().contains(&query)
                    || site.hostname.contains(&query)
                    || site.aliases.iter().any(|alias| alias.contains(&query))
            })
            .collect();
        let folders = if count == 1 { "folder" } else { "folders" };
        let summary = if query.is_empty() {
            format!("{count} {folders} detected in {}", self.snapshot.www.display())
        } else {
            format!("Showing {} of {count} {folders}", shown.len())
        };
        let rows: Vec<AnyElement> =
            shown.iter().map(|(ix, site)| self.render_site_row(*ix, site, cx).into_any_element()).collect();
        let nothing_matches = count > 0 && rows.is_empty();

        v_flex()
            .gap_5()
            .child(
                page_header("Sites", summary, cx).child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(220.)).child(Input::new(&self.site_search).cleanable(true)))
                        .child(
                            Button::new("open-www")
                                .label("Open sites folder")
                                .icon(IconName::FolderOpen)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    cx.open_with_system(&this.snapshot.www)
                                })),
                        )
                        .child(
                            Button::new("new-site")
                                .primary()
                                .label("New site")
                                .icon(IconName::Plus)
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.open_new_site(window, cx);
                                })),
                        ),
                ),
            )
            .child(
                card(cx)
                    .child(table_head(["Site", "PHP", "Document root", ""], cx))
                    .children(rows)
                    .when(nothing_matches, |card| {
                        card.child(empty_row("No sites match the search.", cx))
                    })
                    .when(count == 0, |card| {
                        card.child(empty_row("No sites yet. Create a folder in the sites folder.", cx))
                    }),
            )
    }

    fn render_site_row(&self, ix: usize, site: &Site, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let url = self.snapshot.site_url(site);
        let php = self.php_for(site).map_or("None".to_string(), |php| php.short_version());
        let document_root = format!("/{}", relative_root(site));
        let name = site.name.clone();

        row(cx)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::MEDIUM).child(site.name.clone()))
                    .child(div().text_xs().text_color(muted).child(match self.snapshot.shares.get(&site.name) {
                        Some(public) => format!("{url}  ·  shared at {public}"),
                        None => url.clone(),
                    })),
            )
            .child(cell().child(php))
            .child(cell().child(document_root))
            .child(
                actions()
                    .child(
                        Button::new(("site-open", ix))
                            .small()
                            .label("Open")
                            .icon(IconName::ExternalLink)
                            .disabled(!self.apache_running())
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(
                        Button::new(("site-settings", ix))
                            .small()
                            .label("Settings")
                            .icon(IconName::Settings)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.open_site(name.clone(), window, cx);
                            })),
                    ),
            )
    }
}

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.quitting {
            let theme = cx.theme();
            return v_flex()
                .size_full()
                .bg(theme.background)
                .text_color(theme.foreground)
                .child(
                    TitleBar::new().child(
                        h_flex()
                            .gap_2()
                            .child(img(self.logo.clone()).size_4())
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("OmniPHP")),
                    ),
                )
                .child(self.render_quitting(cx));
        }
        let page = match self.page {
            Page::Services => self.render_services(cx).into_any_element(),
            Page::Sites => self.render_sites(cx).into_any_element(),
            Page::Site => self.render_site(cx),
            Page::NewSite => self.render_new_site(cx),
            Page::Settings => self.render_settings(cx),
        };
        let theme = cx.theme();

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .child(img(self.logo.clone()).size_4())
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("OmniPHP")),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .id("page")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .p_6()
                            .gap_4()
                            .children(self.render_error(cx))
                            .children(self.render_notice(cx))
                            .children(self.render_setup(cx))
                            .child(page),
                    ),
            )
    }
}

// ---- small layout pieces shared by the pages -----------------------------

const CELL_WIDTH: f32 = 150.;
const ACTIONS_WIDTH: f32 = 210.;

fn page_header(title: &'static str, summary: impl Into<SharedString>, cx: &App) -> Div {
    h_flex().justify_between().gap_3().child(
        v_flex()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(summary.into())),
    )
}

fn card(cx: &App) -> Div {
    v_flex().border_1().border_color(cx.theme().border).rounded_lg().overflow_hidden()
}

/// Column titles: the first column stretches, the middle two are fixed, the
/// last sits over the row actions.
fn table_head(titles: [&'static str; 4], cx: &App) -> Div {
    let theme = cx.theme();
    let [first, second, third, last] = titles;
    h_flex()
        .gap_3()
        .px_4()
        .py_2()
        .bg(theme.muted)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .child(div().flex_1().min_w_0().child(first))
        .child(div().w(px(CELL_WIDTH)).flex_shrink_0().child(second))
        .child(div().w(px(CELL_WIDTH)).flex_shrink_0().child(third))
        .child(div().w(px(ACTIONS_WIDTH)).flex_shrink_0().child(last))
}

fn row(cx: &App) -> Div {
    h_flex().gap_3().px_4().py_2p5().border_t_1().border_color(cx.theme().border).text_sm()
}

fn cell() -> Div {
    div().w(px(CELL_WIDTH)).flex_shrink_0()
}

fn actions() -> Div {
    h_flex().w(px(ACTIONS_WIDTH)).flex_shrink_0().gap_1p5().justify_end()
}

/// A site's document root relative to its folder, with forward slashes; empty
/// when it is the folder itself.
fn relative_root(site: &Site) -> String {
    site.document_root
        .strip_prefix(&site.dir)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// A titled card on the site settings page.
fn section(title: impl Into<SharedString>, cx: &App) -> Div {
    card(cx)
        .p_4()
        .gap_3()
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(title.into()))
}

/// One line of the Settings page: title and explanation on the left, the
/// control on the right.
fn setting_row(title: &'static str, help: &'static str, control: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .justify_between()
        .gap_4()
        .px_4()
        .py_2p5()
        .border_b_1()
        .border_color(cx.theme().border)
        .text_sm()
        .child(
            v_flex()
                .child(div().font_weight(FontWeight::MEDIUM).child(title))
                .when(!help.is_empty(), |this| {
                    this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(help))
                }),
        )
        .child(control)
}

/// A labelled form field; the control is added as a child.
fn field(label: &'static str, cx: &App) -> Div {
    v_flex()
        .gap_1p5()
        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label))
}

fn empty_row(message: &'static str, cx: &App) -> Div {
    row(cx).text_color(cx.theme().muted_foreground).child(message)
}
