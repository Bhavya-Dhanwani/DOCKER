//! The eframe app: owns the data, polls the background workers, lays out sidebar / tabs / status bar.
use crate::{
    console::{self, Console, ConsoleAction, StatsView},
    catalog_dialog::{self, CatalogDialog, CatalogOutcome},
    engine,
    dialogs::{self, BuildDialog, Dialog, Outcome, RunDialog},
    group_dialog::{self, GroupDialog, GroupOutcome},
    groups::{self, GroupRun},
    format::fmt_bytes,
    parse,
    state::{Cmd, Data, Kind, Level, Tab, Task, Toast, prune_toasts},
    tabs::{containers, images, live, resources},
    usage,
    theme,
    worker::{self, Event, Worker, run_cli, run_steps},
};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

pub struct App {
    w: Worker,
    rx: Receiver<Event>,
    tab: Tab,
    dark: bool,
    data: Data,
    last: HashMap<Kind, Instant>,
    pending: HashSet<Kind>,
    busy: Vec<String>,
    toasts: Vec<Toast>,
    console: Option<Console>,
    stats: Option<StatsView>,
    dialog: Option<Dialog>,
    confirm: Option<(String, Task)>,
    next_stream: u64,
    cui: containers::UiState,
    iui: images::UiState,
    rui: resources::UiState,
    lui: live::UiState,
    groups_path: Option<PathBuf>,
    group_run: Option<GroupRun>,
    /// no group pruning before this moment (new containers may not be in `ps` yet)
    prune_after: Instant,
    usage_pending: bool,
    usage_started: Option<Instant>,
    pull_queue: VecDeque<String>,
    minimized: bool,
    version_started: Option<Instant>,
    version_pending: bool,
    logo: egui::TextureHandle,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let dark = cc.egui_ctx.style().visuals.dark_mode;
        theme::apply(&cc.egui_ctx, dark);
        let (w, rx) = Worker::new(cc.egui_ctx.clone());
        let groups_path = groups::default_path();
        let data = Data { groups: groups_path.as_deref().map(groups::load).unwrap_or_default(), ..Data::default() };
        App {
            w,
            rx,
            tab: if std::env::var("SASTA_TAB").is_ok_and(|t| t == "live") { Tab::Live } else { Tab::Containers },
            dark,
            data,
            last: HashMap::new(),
            pending: HashSet::new(),
            busy: vec![],
            toasts: vec![],
            console: None,
            stats: None,
            dialog: None,
            confirm: None,
            next_stream: 0,
            cui: Default::default(),
            iui: Default::default(),
            rui: Default::default(),
            lui: Default::default(),
            groups_path,
            group_run: None,
            prune_after: Instant::now(),
            usage_pending: false,
            usage_started: None,
            pull_queue: VecDeque::new(),
            minimized: false,
            version_started: None,
            version_pending: false,
            logo: cc.egui_ctx.load_texture("logo", crate::LOGO.to_color_image(), Default::default()),
        }
    }

    fn toast(&mut self, level: Level, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), level, at: Instant::now() });
    }

    fn tab_kind(&self) -> Kind {
        match self.tab {
            Tab::Containers | Tab::Live => Kind::Containers,
            Tab::Images => Kind::Images,
            Tab::Volumes => Kind::Volumes,
            Tab::Networks => Kind::Networks,
        }
    }

    /// Ask every list to reload on the next frame.
    fn invalidate(&mut self) {
        self.last.clear();
    }

    fn fetch(&mut self, kind: Kind) {
        self.last.insert(kind, Instant::now());
        self.pending.insert(kind);
        let a = |x: &[&str]| worker::args(x);
        match kind {
            Kind::Containers => self.w.spawn(move || Event::Containers(run_cli(&a(&["ps", "-a", "--no-trunc", "--format", "{{json .}}"])).map(|o| parse::parse_ps_json(&o)))),
            Kind::Images => self.w.spawn(move || Event::Images(run_cli(&a(&["images", "--format", "{{json .}}"])).map(|o| parse::parse_images_json(&o)))),
            Kind::Volumes => self.w.spawn(move || Event::Volumes(run_cli(&a(&["volume", "ls"])).map(|o| parse::parse_volumes(&o)))),
            Kind::Networks => self.w.spawn(move || Event::Networks(run_cli(&a(&["network", "ls"])).map(|o| parse::parse_networks(&o)))),
            Kind::Df => self.w.spawn(move || Event::Df(run_cli(&a(&["system", "df"])).map(|o| parse::parse_df(&o)))),
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        // every list is polled (the sidebar shows all the counts); the open tab a little faster
        let open = self.tab_kind();
        let due = [Kind::Containers, Kind::Images, Kind::Volumes, Kind::Networks, Kind::Df].map(|k| (k, if k == Kind::Containers || k == open { 2 } else { 6 }));
        for (kind, secs) in due {
            if !self.pending.contains(&kind) && self.last.get(&kind).is_none_or(|t| now.duration_since(*t) >= Duration::from_secs(secs)) {
                self.fetch(kind);
            }
        }
        if let Some(s) = &mut self.stats
            && !s.pending
            && s.polled.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2))
        {
            s.pending = true;
            s.polled = Some(now);
            let id = s.id.clone();
            self.w.spawn(move || Event::Stats(id.clone(), run_cli(&worker::args(&["stats", "--no-stream", &id])).and_then(|o| parse::parse_stats(&o).ok_or_else(|| "container is not running".to_string()))));
        }
        self.poll_version(now);
        self.poll_usage(now);
        prune_toasts(&mut self.toasts, now);
    }

    /// One `docker stats --no-stream --json` at a time, about once a second; slower when nothing runs.
    fn poll_usage(&mut self, now: Instant) {
        if self.usage_pending || self.minimized {
            return;
        }
        let any_running = self.data.containers.iter().any(|c| c.state == parse::State::Running);
        let every = if any_running || self.data.usage.err.is_some() || self.tab == Tab::Live { Duration::from_millis(1000) } else { Duration::from_secs(5) };
        if self.usage_started.is_some_and(|t| now.duration_since(t) < every) {
            return;
        }
        self.usage_pending = true;
        self.usage_started = Some(now);
        self.w.spawn(|| Event::Usage(run_cli(&worker::args(&["stats", "--no-stream", "--json"])).map(|o| usage::parse_json_lines(&o))));
    }

    /// `docker version` at startup and every ~30 s: tells native Windows mode from a Linux VM.
    fn poll_version(&mut self, now: Instant) {
        if self.version_pending || self.version_started.is_some_and(|t| now.duration_since(t) < Duration::from_secs(30)) {
            return;
        }
        self.version_pending = true;
        self.version_started = Some(now);
        self.w.spawn(|| Event::Version(run_cli(&worker::args(&["version"]))));
    }

    fn save_groups(&mut self) {
        if let Some(p) = &self.groups_path
            && let Err(e) = groups::save(p, &self.data.groups)
        {
            self.toast(Level::Err, format!("Cannot save groups: {e}"));
        }
    }

    fn store<T>(&mut self, kind: Kind, r: Result<T, String>, set: impl FnOnce(&mut Data, T)) {
        self.pending.remove(&kind);
        self.data.loaded.insert(kind);
        match r {
            Ok(v) => {
                self.data.errors.remove(&kind);
                set(&mut self.data, v);
            }
            Err(e) => {
                self.data.errors.insert(kind, e);
            }
        }
    }

    fn on_event(&mut self, e: Event) {
        match e {
            Event::Containers(r) => {
                self.store(Kind::Containers, r, |d, v| d.containers = v);
                self.data.sync_paused();
                if !self.data.errors.contains_key(&Kind::Containers) {
                    let ids: Vec<&str> = self.data.containers.iter().map(|c| c.id.as_str()).collect();
                    self.data.usage.retain_ids(&ids);
                    let names: Vec<&str> = self.data.containers.iter().map(|c| c.name.as_str()).collect();
                    if self.group_run.as_ref().is_none_or(|r| r.done) && Instant::now() >= self.prune_after && groups::prune(&mut self.data.groups, &names) {
                        self.save_groups();
                    }
                }
            }
            Event::Version(r) => {
                self.version_pending = false;
                if let Ok(out) = r {
                    self.data.native = engine::parse_native(&out);
                }
            }
            Event::Usage(r) => {
                self.usage_pending = false;
                match r {
                    Ok(v) => self.data.usage.ingest(v, Instant::now()),
                    Err(e) => self.data.usage.fail(e.lines().next().unwrap_or("").to_string()),
                }
            }
            Event::GroupStep { group, idx, result } => {
                if let Some(r) = self.group_run.as_mut().filter(|r| r.name == group) {
                    r.apply(idx, result);
                }
                self.invalidate();
            }
            Event::GroupDone(group) => {
                if let Some(r) = self.group_run.as_mut().filter(|r| r.name == group) {
                    r.finish();
                    let ok = r.succeeded();
                    if !ok.is_empty() {
                        groups::upsert(&mut self.data.groups, &group, &ok);
                        self.prune_after = Instant::now() + groups::PRUNE_GRACE;
                        self.save_groups();
                    }
                }
                self.invalidate();
            }
            Event::Images(r) => self.store(Kind::Images, r, |d, v| d.images = v),
            Event::Volumes(r) => self.store(Kind::Volumes, r, |d, v| d.volumes = v),
            Event::Networks(r) => self.store(Kind::Networks, r, |d, v| d.networks = v),
            Event::Df(r) => self.store(Kind::Df, r, |d, v| d.df = Some(v)),
            Event::Stats(id, r) => {
                if let Some(s) = self.stats.as_mut().filter(|s| s.id == id) {
                    s.pending = false;
                    match r {
                        Ok(v) => (s.last, s.err) = (Some(v), None),
                        Err(e) => s.err = Some(e),
                    }
                }
            }
            Event::Done { label, steps, result } => {
                if let Some(i) = self.busy.iter().position(|b| *b == label) {
                    self.busy.remove(i);
                }
                match result {
                    Ok(_) => {
                        // `docker ps` cannot tell paused from running, so remember it ourselves
                        for s in &steps {
                            match (s.first().map(String::as_str), s.get(1)) {
                                (Some("pause"), Some(id)) => drop(self.data.paused.insert(id.clone())),
                                (Some("unpause" | "stop" | "kill" | "restart" | "rm"), Some(id)) => drop(self.data.paused.remove(id)),
                                _ => {}
                            }
                        }
                        self.toast(Level::Ok, format!("✔ {label}"));
                    }
                    Err(e) => self.toast(Level::Err, format!("{label} failed: {e}")),
                }
                self.invalidate();
            }
            Event::Line(id, line) => {
                if let Some(c) = self.console.as_mut().filter(|c| c.id == id) {
                    c.push(line);
                }
            }
            Event::End(id, ok) => {
                let Some(c) = self.console.as_mut().filter(|c| c.id == id) else { return };
                c.running = false;
                c.ok = Some(ok);
                let (title, is_logs) = (c.title.clone(), c.logs_of.is_some());
                if title.starts_with("Pull ")
                    && let Some(next) = self.pull_queue.pop_front()
                {
                    let carry = self.console.take().map(|mut c| std::mem::take(&mut c.lines)).unwrap_or_default();
                    self.start_pull(next, carry);
                    self.invalidate();
                    return;
                }
                if !is_logs {
                    self.toast(if ok { Level::Ok } else { Level::Err }, format!("{title}: {}", if ok { "finished" } else { "failed (see output)" }));
                }
                self.invalidate();
            }
            Event::Picked(path) => {
                if let (false, Some(Dialog::Run(d))) = (path.is_empty(), &mut self.dialog) {
                    d.folder.folder = path;
                } else if let (false, Some(Dialog::Build(d))) = (path.is_empty(), &mut self.dialog) {
                    if d.spec.tag.trim().is_empty() {
                        d.spec.tag = dialogs::default_tag(&path);
                    }
                    d.spec.context = path;
                }
            }
        }
    }

    fn run_task(&mut self, t: Task) {
        self.busy.push(t.label.clone());
        let Task { label, steps } = t;
        self.w.spawn(move || {
            let result = run_steps(&steps);
            Event::Done { label, steps, result }
        });
    }

    fn start_console(&mut self, title: String, args: Vec<String>, logs_of: Option<(String, String)>, follow: bool) {
        self.next_stream += 1;
        let cancel = self.w.stream(self.next_stream, args);
        let mut c = Console::new(self.next_stream, title, cancel);
        c.logs_of = logs_of;
        c.follow = follow;
        self.console = Some(c); // dropping the old console stops its stream
    }

    fn start_pull(&mut self, image: String, carry: Vec<String>) {
        let left = self.pull_queue.len();
        self.start_console(format!("Pull {image}"), worker::args(&["pull", &image]), None, true);
        if let Some(c) = &mut self.console {
            c.lines = carry;
            c.push(format!("── pulling {image}{} ──", if left > 0 { format!(" ({left} more queued)") } else { String::new() }));
        }
    }

    fn start_logs(&mut self, id: String, name: String, follow: bool) {
        let args = if follow { worker::args(&["logs", "-f", &id]) } else { worker::args(&["logs", &id]) };
        self.start_console(format!("Logs: {name}"), args, Some((id, name)), follow);
    }

    fn apply(&mut self, ctx: &egui::Context, cmds: Vec<Cmd>) {
        for c in cmds {
            match c {
                Cmd::Task(t) => self.run_task(t),
                Cmd::Confirm(m, t) => self.confirm = Some((m, t)),
                Cmd::Logs { id, name } => self.start_logs(id, name, true),
                Cmd::Stats { id, name } => self.stats = Some(StatsView { id, name, last: None, err: None, pending: false, polled: None }),
                Cmd::Pull(n) => {
                    self.pull_queue.clear();
                    self.start_pull(n, vec![]);
                }
                Cmd::GroupDialog(imgs) => self.dialog = Some(Dialog::Group(GroupDialog::new(&imgs))),
                Cmd::CatalogDialog => self.dialog = Some(Dialog::Catalog(CatalogDialog::default())),
                Cmd::CopyNote(text, note) => {
                    ctx.copy_text(text);
                    self.toast(Level::Ok, note);
                }
                Cmd::RunDialog(img) => self.dialog = Some(Dialog::Run(Box::new(RunDialog::for_image(&img)))),
                Cmd::BuildDialog => self.dialog = Some(Dialog::Build(BuildDialog::default())),
                Cmd::Copy(text) => {
                    ctx.copy_text(text.clone());
                    self.toast(Level::Info, format!("Copied: {text}"));
                }
            }
        }
    }

    fn windows(&mut self, ctx: &egui::Context) {
        if let Some(mut d) = self.dialog.take() {
            let outcome = match &mut d {
                Dialog::Run(r) => dialogs::show_run(ctx, r, &self.w, self.data.native.is_some()),
                Dialog::Build(b) => dialogs::show_build(ctx, b, &self.w),
                Dialog::Group(g) => match group_dialog::show(ctx, g) {
                    GroupOutcome::Open => Outcome::Open,
                    GroupOutcome::Cancel => Outcome::Cancel,
                    GroupOutcome::Run { group, network, runs, members } => {
                        self.group_run = Some(GroupRun::new(&group, &members));
                        self.w.run_group(group, network, runs);
                        Outcome::Cancel
                    }
                },
                Dialog::Catalog(c) => {
                    let local: Vec<String> = self.data.images.iter().map(|i| i.name.clone()).collect();
                    match catalog_dialog::show(ctx, c, &local) {
                        CatalogOutcome::Open => Outcome::Open,
                        CatalogOutcome::Cancel => Outcome::Cancel,
                        CatalogOutcome::Pull(mut list) => {
                            let first = list.remove(0);
                            self.pull_queue = list.into();
                            self.start_pull(first, vec![]);
                            Outcome::Cancel
                        }
                    }
                }
            };
            match outcome {
                Outcome::Open => self.dialog = Some(d),
                Outcome::Cancel => {}
                Outcome::Submit { title, args, detach } => {
                    if detach {
                        self.run_task(Task { label: title, steps: vec![args] });
                    } else {
                        self.start_console(title, args, None, true);
                    }
                }
            }
        }
        if self.group_run.as_ref().is_some_and(|r| !group_dialog::show_progress(ctx, r)) {
            self.group_run = None;
        }
        if let Some((msg, task)) = self.confirm.take() {
            match dialogs::show_confirm(ctx, &msg) {
                None => self.confirm = Some((msg, task)),
                Some(true) => self.run_task(task),
                Some(false) => {}
            }
        }
        if let Some(c) = &mut self.console {
            match console::show_console(ctx, c) {
                ConsoleAction::None => {}
                ConsoleAction::Close => self.console = None,
                ConsoleAction::Restream => {
                    if let Some((id, name)) = c.logs_of.clone() {
                        let follow = c.follow;
                        self.start_logs(id, name, follow);
                    }
                }
            }
        }
        if self.stats.as_ref().is_some_and(|s| !console::show_stats(ctx, s)) {
            self.stats = None;
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), egui::vec2(38.0, 38.0))).tint(ui.visuals().text_color()));
            ui.label(RichText::new("Sasta Docker").size(18.0).strong());
        });
        ui.add_space(12.0);
        let d = &self.data;
        let running = d.containers.iter().filter(|c| c.state == parse::State::Running).count();
        let items = [
            (Tab::Containers, "📦 Containers", format!("{running}/{}", d.containers.len())),
            (Tab::Images, "🖼 Images", d.images.len().to_string()),
            (Tab::Volumes, "💾 Volumes", d.volumes.len().to_string()),
            (Tab::Networks, "🌐 Networks", d.networks.len().to_string()),
            (Tab::Live, "📈 Live", running.to_string()),
        ];
        for (tab, label, count) in items {
            let text = RichText::new(format!("{label}   {count}")).size(15.0);
            if ui.add_sized([ui.available_width(), 32.0], egui::SelectableLabel::new(self.tab == tab, text)).clicked() {
                self.tab = tab;
                self.last.remove(&self.tab_kind());
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.add_space(8.0);
            if ui.button(if self.dark { "☀ Light theme" } else { "🌙 Dark theme" }).clicked() {
                self.dark = !self.dark;
                theme::apply(ui.ctx(), self.dark);
            }
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let engine_ok = (self.data.native.is_some() || !self.data.errors.contains_key(&Kind::Containers)) && self.data.is_loaded(Kind::Containers);
            ui.label(RichText::new("●").color(if engine_ok { theme::GREEN } else { theme::RED }));
            let df = self.data.df.clone().unwrap_or_default();
            let disk = df.image_bytes + df.volume_bytes;
            ui.label(format!(
                "{} containers ({} running)   ·   {} images   ·   {} volumes   ·   Disk used by store: {} (images {}, volumes {})",
                self.data.containers.len().max(df.containers),
                self.data.containers.iter().filter(|c| c.state == parse::State::Running).count(),
                df.images.max(self.data.images.len()),
                df.volumes,
                fmt_bytes(disk),
                fmt_bytes(df.image_bytes),
                fmt_bytes(df.volume_bytes),
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(b) = self.busy.last() {
                    ui.label(RichText::new(format!("{b}…")).weak());
                    ui.spinner();
                }
            });
        });
    }

    fn toasts_ui(&self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -44.0]).order(egui::Order::Foreground).show(ctx, |ui| {
            ui.set_max_width(380.0);
            for t in &self.toasts {
                let fill = match t.level {
                    Level::Info => theme::ACCENT,
                    Level::Ok => theme::GREEN,
                    Level::Err => theme::RED,
                };
                egui::Frame::popup(ui.style()).fill(fill).show(ui, |ui| ui.label(RichText::new(&t.text).color(Color32::WHITE)));
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        while let Ok(e) = self.rx.try_recv() {
            self.on_event(e);
        }
        self.minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        self.tick();
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::SidePanel::left("nav").exact_width(190.0).resizable(false).show(ctx, |ui| self.sidebar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));

        let mut cmds = vec![];
        let mut retry = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.data.native.is_some() && !self.data.banner_dismissed {
                egui::Frame::new().fill(theme::AMBER.gamma_multiply(0.3)).corner_radius(6).inner_margin(10).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(engine::BANNER).strong());
                    if let Some(why) = self.data.native.as_deref().filter(|w| !w.is_empty()) {
                        ui.label(RichText::new(format!("Reason: {why}")).weak().small());
                    }
                    ui.horizontal(|ui| {
                        if ui.button("📋 Copy command").clicked() {
                            ui.ctx().copy_text(engine::INSTALL_COMMAND.to_string());
                        }
                        if ui.button("Dismiss").clicked() {
                            self.data.banner_dismissed = true;
                        }
                    });
                });
                ui.add_space(6.0);
            }
            // native mode: empty lists are normal, the engine is not "unavailable"
            let err = if self.data.native.is_some() { None } else { self.data.errors.get(&Kind::Containers).or_else(|| self.data.errors.get(&self.tab_kind())) };
            if let Some(e) = err {
                egui::Frame::new().fill(theme::RED.gamma_multiply(0.25)).corner_radius(6).inner_margin(10).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(format!("Cannot talk to the engine: {e}")).color(theme::RED));
                    ui.label("On Windows the Linux VM must exist first: run `docker vm init` once in a terminal.");
                    retry = ui.button("Retry now").clicked();
                });
                ui.add_space(6.0);
            }
            match self.tab {
                Tab::Containers => containers::show(ui, &self.data, &mut self.cui, &mut cmds),
                Tab::Images => images::show(ui, &self.data, &mut self.iui, &mut cmds),
                Tab::Volumes => resources::volumes(ui, &self.data, &mut self.rui, &mut cmds),
                Tab::Networks => resources::networks(ui, &self.data, &mut self.rui, &mut cmds),
                Tab::Live => live::show(ui, &self.data, &mut self.lui),
            }
        });
        if retry {
            self.invalidate();
        }
        self.apply(ctx, cmds);
        self.windows(ctx);
        self.toasts_ui(ctx);
    }
}
