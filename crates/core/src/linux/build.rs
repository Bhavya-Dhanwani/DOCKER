//! `docker build`: Dockerfile -> layers.
//!
//! * `RUN` executes in a container whose overlayfs *upper directory* becomes the new layer
//!   (the same layout the image store already uses, so no tarballs are involved).
//! * `COPY` writes its files straight into a new layer directory, from the build context
//!   (honouring `.dockerignore`) or from another stage / image (`--from`), optionally `--chown`ed.
//! * Every layer is stored under a key = hash(parent key, instruction, inputs). That key is also the
//!   build cache: if the layer directory exists, the step is skipped.
//! * A Dockerfile may have several stages (`FROM x AS name`); the last one (or `--target`) is the image.
use super::{run_step, Step};
use crate::{cli::BuildArgs, error::{Error, Result}, registry, user};
use docker_image::{
    dir_size,
    dockerfile::{self, Cmd, Instruction},
    Health, Ignore, ImageMeta, LayerRef, Reference, Store,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::CString,
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{symlink, FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

const LAYER_TYPE: &str = "application/vnd.rustdocker.layer.v1+dir";

fn img(e: docker_image::Error) -> Error {
    Error::Usage(e.to_string())
}

fn io_err(ctx: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let ctx = ctx.into();
    move |e| Error::io(ctx, e)
}

/// Hex sha256 over the parts (NUL separated).
fn key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    format!("{:x}", h.finalize())
}

/// Resolve `p` against `workdir` and collapse `.`/`..`: always absolute, no trailing slash.
fn norm(workdir: &str, p: &str) -> String {
    let joined = if p.starts_with('/') { p.to_string() } else { format!("{workdir}/{p}") };
    let mut out: Vec<&str> = vec![];
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    format!("/{}", out.join("/"))
}

fn join_in(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.trim_start_matches('/'))
}

fn argv(c: &Cmd) -> Vec<String> {
    match c {
        Cmd::Shell(s) => vec!["/bin/sh".into(), "-c".into(), s.clone()],
        Cmd::Exec(v) => v.clone(),
    }
}

fn set_env(env: &mut Vec<String>, k: &str, v: &str) {
    let prefix = format!("{k}=");
    match env.iter().position(|e| e.starts_with(&prefix)) {
        Some(i) => env[i] = format!("{k}={v}"),
        None => env.push(format!("{k}={v}")),
    }
}

#[derive(Clone)]
struct State {
    layers: Vec<LayerRef>,
    /// Key of the newest layer (the cache chain).
    chain: String,
    env: Vec<String>,
    workdir: String,
    user: String,
    entrypoint: Vec<String>,
    cmd: Vec<String>,
    exposed: Vec<String>,
    labels: BTreeMap<String, String>,
    args: BTreeMap<String, String>,
    health: Option<Health>,
}

impl State {
    /// Variables available for `$X` substitution: ARGs, overridden by ENV.
    fn vars(&self) -> HashMap<String, String> {
        let mut v: HashMap<String, String> = self.args.clone().into_iter().collect();
        for kv in &self.env {
            if let Some((k, val)) = kv.split_once('=') {
                v.insert(k.into(), val.into());
            }
        }
        v
    }

    /// Environment for a RUN: ENV plus any ARG not shadowed by an ENV.
    fn run_env(&self) -> Vec<String> {
        let mut e = self.env.clone();
        for (k, v) in &self.args {
            if !e.iter().any(|x| x.starts_with(&format!("{k}="))) {
                e.push(format!("{k}={v}"));
            }
        }
        e
    }

    fn layer_paths(&self, store: &Store) -> Result<Vec<PathBuf>> {
        self.layers.iter().map(|l| store.layer_path(&l.digest).map_err(img)).collect()
    }
}

type Stages = Vec<(Option<String>, State)>;

fn from(store: &Store, image: &str, say: &mut dyn FnMut(String)) -> Result<State> {
    if image == "scratch" {
        return Ok(State {
            layers: vec![],
            chain: key(&["scratch"]),
            env: vec![],
            workdir: "/".into(),
            user: String::new(),
            entrypoint: vec![],
            cmd: vec![],
            exposed: vec![],
            labels: BTreeMap::new(),
            args: BTreeMap::new(),
            health: None,
        });
    }
    let r = Reference::parse(image).map_err(img)?;
    let meta = match store.get(&r).map_err(img)? {
        Some(m) => m,
        None => {
            say(format!(" ---> Pulling {}", r.display()));
            docker_image::pull(store, &r, &mut |l| say(l)).map_err(img)?
        }
    };
    for l in &meta.layers {
        store.ensure_layer(l).map_err(img)?;
    }
    let digests: Vec<&str> = meta.layers.iter().map(|l| l.digest.as_str()).collect();
    Ok(State {
        chain: key(&["from", &digests.join(",")]),
        layers: meta.layers.clone(),
        env: meta.env.clone(),
        workdir: if meta.working_dir.is_empty() { "/".into() } else { meta.working_dir.clone() },
        user: meta.user.clone(),
        entrypoint: meta.entrypoint.clone(),
        cmd: meta.cmd.clone(),
        exposed: meta.exposed.clone(),
        labels: meta.labels.clone(),
        args: BTreeMap::new(),
        health: meta.healthcheck.clone(),
    })
}

/// Make (or reuse) the layer `key`. `produce` fills a staging directory and returns the directory to adopt.
fn commit(
    store: &Store,
    st: &mut State,
    key: String,
    no_cache: bool,
    say: &mut dyn FnMut(String),
    produce: impl FnOnce(&Path) -> Result<PathBuf>,
) -> Result<()> {
    let digest = format!("sha256:{key}");
    let path = store.layer_path(&digest).map_err(img)?;
    if !no_cache && path.is_dir() {
        say(" ---> Using cache".into());
    } else {
        let stage = store.new_staging_dir("layer").map_err(img)?;
        let dir = produce(&stage)?;
        store.adopt_layer(&digest, &dir).map_err(img)?;
        let _ = fs::remove_dir_all(&stage);
        say(format!(" ---> {}", &key[..12]));
    }
    st.layers.push(LayerRef { digest, media_type: LAYER_TYPE.into(), size: dir_size(&path) });
    st.chain = key;
    Ok(())
}

/// Remove what the container runtime itself put into the upper dir (mount points, our resolv.conf).
fn scrub(upper: &Path) {
    let rc = upper.join("etc/resolv.conf");
    if let Ok(md) = fs::symlink_metadata(&rc)
        && ((md.is_file() && md.len() == 0) || md.file_type().is_char_device())
    {
        let _ = fs::remove_file(&rc);
    }
    for d in ["etc", "proc", "dev", "tmp", "old"] {
        let _ = fs::remove_dir(upper.join(d)); // only succeeds when empty
    }
}

// ---- reading another stage / image: a merged, read-only view of overlay layers

fn is_whiteout(md: &fs::Metadata) -> bool {
    md.file_type().is_char_device() && md.rdev() == 0
}

fn is_opaque(p: &Path) -> bool {
    let c = CString::new(p.as_os_str().as_bytes()).expect("no NUL in path");
    let mut b = [0u8; 1];
    let n = unsafe { libc::lgetxattr(c.as_ptr(), c"trusted.overlay.opaque".as_ptr(), b.as_mut_ptr().cast(), 1) };
    n == 1 && b[0] == b'y'
}

/// What the image looks like with all layers applied (whiteouts hide, opaque directories cut off lower layers).
struct Merged<'a> {
    layers: &'a [PathBuf], // bottom first
}

impl Merged<'_> {
    /// The on-disk entry for `rel`, or None if it does not exist in the merged view.
    fn locate(&self, rel: &Path) -> Option<PathBuf> {
        let mut lo = 0; // layers below this index are hidden by an opaque directory
        let mut prefix = PathBuf::new();
        let comps: Vec<_> = rel.components().filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_owned()),
            _ => None,
        }).collect();
        if comps.is_empty() {
            return self.layers.last().cloned();
        }
        let mut found = PathBuf::new();
        for (i, c) in comps.iter().enumerate() {
            prefix.push(c);
            let (idx, p, md) = (lo..self.layers.len()).rev().find_map(|k| {
                let p = self.layers[k].join(&prefix);
                fs::symlink_metadata(&p).ok().map(|md| (k, p, md))
            })?;
            if is_whiteout(&md) {
                return None;
            }
            if i + 1 < comps.len() {
                if !md.is_dir() {
                    return None;
                }
                if is_opaque(&p) {
                    lo = idx;
                }
            }
            found = p;
        }
        Some(found)
    }

    /// Names inside the merged directory `rel`.
    fn list(&self, rel: &Path) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut hidden: BTreeSet<String> = BTreeSet::new();
        // only layers that can contribute: stop below the first opaque layer
        for l in self.layers.iter().rev() {
            let dir = l.join(rel);
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if hidden.contains(&name) {
                    continue;
                }
                match e.metadata() {
                    Ok(md) if is_whiteout(&md) => {
                        hidden.insert(name);
                    }
                    _ => {
                        names.insert(name.clone());
                        hidden.insert(name); // a lower layer's entry of the same name is shadowed
                    }
                }
            }
            if is_opaque(&dir) {
                break;
            }
        }
        names
    }

    fn copy(&self, rel: &Path, dst: &Path) -> Result<()> {
        let src = self.locate(rel).ok_or_else(|| Error::Usage(format!("COPY failed: /{} not found in the source stage", rel.display())))?;
        let md = fs::symlink_metadata(&src).map_err(io_err("stat"))?;
        if md.is_dir() {
            fs::create_dir_all(dst).map_err(io_err(format!("mkdir {}", dst.display())))?;
            let _ = fs::set_permissions(dst, md.permissions());
            for name in self.list(rel) {
                self.copy(&rel.join(&name), &dst.join(&name))?;
            }
            Ok(())
        } else {
            copy_entry(&src, dst)
        }
    }

    fn is_dir(&self, rel: &Path) -> bool {
        self.locate(rel).and_then(|p| fs::symlink_metadata(p).ok()).is_some_and(|m| m.is_dir())
    }
}

// ---- COPY

fn resolve_src(ctx: &Path, s: &str) -> Result<PathBuf> {
    let rel = s.trim_start_matches('/');
    let p = ctx.join(if rel.is_empty() { "." } else { rel });
    let c = fs::canonicalize(&p).map_err(|_| Error::Usage(format!("COPY failed: no source files found: {s}")))?;
    if !c.starts_with(ctx) {
        return Err(Error::Usage(format!("COPY failed: {s} is outside the build context")));
    }
    Ok(c)
}

/// `/`-separated path inside the context, for `.dockerignore` matching.
fn ctx_rel(ctx: &Path, p: &Path) -> String {
    p.strip_prefix(ctx).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Is this directory skippable? With `!` rules an ignored directory may still contain wanted files.
fn skip_dir(ig: &Ignore, rel: &str) -> bool {
    ig.is_ignored(rel) && !ig.has_negations()
}

fn hash_tree(h: &mut Sha256, ctx: &Path, ig: &Ignore, path: &Path, rel: &str) -> Result<()> {
    let md = fs::symlink_metadata(path).map_err(io_err(format!("stat {}", path.display())))?;
    let ctx_path = ctx_rel(ctx, path);
    if !md.is_dir() && ig.is_ignored(&ctx_path) {
        return Ok(());
    }
    h.update(rel.as_bytes());
    h.update((md.mode() & 0o7777).to_le_bytes());
    if md.file_type().is_symlink() {
        h.update(b"L");
        h.update(fs::read_link(path).map_err(io_err("readlink"))?.to_string_lossy().as_bytes());
    } else if md.is_dir() {
        h.update(b"D");
        let mut names: Vec<String> = fs::read_dir(path)
            .map_err(io_err(format!("read {}", path.display())))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for n in names {
            let child = path.join(&n);
            let is_dir = fs::symlink_metadata(&child).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir && skip_dir(ig, &ctx_rel(ctx, &child)) {
                continue;
            }
            hash_tree(h, ctx, ig, &child, &format!("{rel}/{n}"))?;
        }
    } else {
        h.update(b"F");
        let mut f = fs::File::open(path).map_err(io_err(format!("open {}", path.display())))?;
        io::copy(&mut f, h).map_err(io_err("hash file"))?;
    }
    Ok(())
}

/// Content hash of everything a COPY will read, so changed files invalidate the cache.
fn hash_sources(ctx: &Path, ig: &Ignore, srcs: &[String]) -> Result<String> {
    let mut h = Sha256::new();
    for s in srcs {
        h.update(s.as_bytes());
        hash_tree(&mut h, ctx, ig, &resolve_src(ctx, s)?, "")?;
    }
    Ok(format!("{:x}", h.finalize()))
}

fn copy_entry(src: &Path, dst: &Path) -> Result<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p).map_err(io_err(format!("mkdir {}", p.display())))?;
    }
    let md = fs::symlink_metadata(src).map_err(io_err(format!("stat {}", src.display())))?;
    if md.file_type().is_symlink() {
        let target = fs::read_link(src).map_err(io_err("readlink"))?;
        let _ = fs::remove_file(dst);
        symlink(target, dst).map_err(io_err(format!("symlink {}", dst.display())))?;
    } else {
        fs::copy(src, dst).map_err(io_err(format!("copy {} -> {}", src.display(), dst.display())))?; // keeps mode bits
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path, ctx: &Path, ig: &Ignore, create: bool) -> Result<()> {
    if create {
        fs::create_dir_all(dst).map_err(io_err(format!("mkdir {}", dst.display())))?;
        let perms = fs::metadata(src).map_err(io_err("stat"))?.permissions();
        let _ = fs::set_permissions(dst, perms);
    }
    for e in fs::read_dir(src).map_err(io_err(format!("read {}", src.display())))?.flatten() {
        let (p, t) = (e.path(), dst.join(e.file_name()));
        let rel = ctx_rel(ctx, &p);
        if e.file_type().map_err(io_err("stat"))?.is_dir() {
            if skip_dir(ig, &rel) {
                continue;
            }
            // an ignored directory that holds `!` re-included files is only created if something lands in it
            copy_tree(&p, &t, ctx, ig, !ig.is_ignored(&rel))?;
        } else if !ig.is_ignored(&rel) {
            copy_entry(&p, &t)?;
        }
    }
    Ok(())
}

/// Does `path` already exist as a directory in the image (so a file copied to it goes *inside*)?
fn is_dir_in_image(lowers: &[PathBuf], path: &str) -> bool {
    Merged { layers: lowers }.is_dir(Path::new(path.trim_start_matches('/')))
}

/// Where COPY reads from.
enum Src<'a> {
    Context(&'a Path, &'a Ignore),
    Layers(Vec<PathBuf>),
}

/// Fill a new layer. Returns the layer directory and the top-level destinations written (for `--chown`).
fn copy_layer(stage: &Path, src: &Src, srcs: &[String], dst: &str, workdir: &str, lowers: &[PathBuf]) -> Result<(PathBuf, Vec<PathBuf>)> {
    let out = stage.join("layer");
    fs::create_dir_all(&out).map_err(io_err("mkdir layer"))?;
    let dst_abs = norm(workdir, dst);
    let into_dir = dst.ends_with('/') || srcs.len() > 1 || is_dir_in_image(lowers, &dst_abs);
    let mut targets = vec![];
    for s in srcs {
        match src {
            Src::Context(ctx, ig) => {
                let p = resolve_src(ctx, s)?;
                if p != **ctx && ig.is_ignored(&ctx_rel(ctx, &p)) {
                    return Err(Error::Usage(format!("COPY failed: {s} is excluded by .dockerignore")));
                }
                let is_dir = fs::symlink_metadata(&p).map_err(io_err("stat"))?.is_dir();
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let target = if is_dir { join_in(&out, &dst_abs) } else if into_dir { join_in(&out, &format!("{dst_abs}/{name}")) } else { join_in(&out, &dst_abs) };
                if is_dir {
                    copy_tree(&p, &target, ctx, ig, true)?;
                } else {
                    copy_entry(&p, &target)?;
                }
                targets.push(target);
            }
            Src::Layers(layers) => {
                let rel = Path::new(s.trim_start_matches('/'));
                let m = Merged { layers };
                let is_dir = m.is_dir(rel);
                let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let target = if is_dir { join_in(&out, &dst_abs) } else if into_dir { join_in(&out, &format!("{dst_abs}/{name}")) } else { join_in(&out, &dst_abs) };
                m.copy(rel, &target)?;
                targets.push(target);
            }
        }
    }
    Ok((out, targets))
}

fn chown_tree(p: &Path, uid: u32, gid: u32) {
    let c = CString::new(p.as_os_str().as_bytes()).expect("no NUL in path");
    unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    if fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
        && let Ok(rd) = fs::read_dir(p)
    {
        for e in rd.flatten() {
            chown_tree(&e.path(), uid, gid);
        }
    }
}

/// `--chown=user[:group]`: names are looked up in the image being built (its /etc/passwd and /etc/group).
fn apply_chown(spec: &str, targets: &[PathBuf], lowers: &[PathBuf]) -> Result<()> {
    let m = Merged { layers: lowers };
    let read = |rel: &str| m.locate(Path::new(rel)).and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
    let ids = user::resolve(spec, &read("etc/passwd"), &read("etc/group"))?;
    for t in targets {
        chown_tree(t, ids.uid, ids.gid);
    }
    Ok(())
}

// ---- the build loop

/// `COPY --from=<stage name | stage index | image>`: the layers to read and an id for the cache key.
fn copy_source(store: &Store, stages: &Stages, from_ref: &str, say: &mut dyn FnMut(String)) -> Result<(Vec<PathBuf>, String)> {
    let found = stages
        .iter()
        .find(|(a, _)| a.as_deref() == Some(from_ref))
        .or_else(|| from_ref.parse::<usize>().ok().and_then(|i| stages.get(i)));
    let st = match found {
        Some((_, s)) => s.clone(),
        None => from(store, from_ref, say)?, // an image reference
    };
    Ok((st.layer_paths(store)?, st.chain))
}

#[allow(clippy::too_many_arguments)]
fn apply(
    store: &Store,
    ctx: &Path,
    ig: &Ignore,
    a: &BuildArgs,
    globals: &HashMap<String, String>,
    stages: &Stages,
    st: &mut State,
    step: &dockerfile::Step,
    say: &mut dyn FnMut(String),
) -> Result<()> {
    match &step.inst {
        Instruction::From { .. } => unreachable!("handled by the caller"),
        Instruction::Arg { name, default } => {
            let given = a.args.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            if let Some(v) = given.or_else(|| default.clone()).or_else(|| globals.get(name).cloned()) {
                st.args.insert(name.clone(), v);
            }
        }
        Instruction::Env(pairs) => {
            for (k, v) in pairs {
                let v = dockerfile::subst(v, &st.vars());
                set_env(&mut st.env, k, &v);
            }
        }
        Instruction::Label(pairs) => {
            for (k, v) in pairs {
                let v = dockerfile::subst(v, &st.vars());
                st.labels.insert(k.clone(), v);
            }
        }
        Instruction::Expose(ports) => {
            for p in ports {
                let p = dockerfile::subst(p, &st.vars());
                let p = if p.contains('/') { p } else { format!("{p}/tcp") };
                if !st.exposed.contains(&p) {
                    st.exposed.push(p);
                }
            }
        }
        Instruction::User(u) => st.user = dockerfile::subst(u, &st.vars()),
        Instruction::Cmd(c) => st.cmd = argv(c),
        Instruction::Entrypoint(c) => {
            st.entrypoint = argv(c).into_iter().filter(|_| !matches!(c, Cmd::Exec(v) if v.is_empty())).collect();
            st.cmd.clear(); // like Docker: a new ENTRYPOINT drops the base image's CMD
        }
        Instruction::Healthcheck(spec) => {
            st.health = spec.as_ref().map(|h| {
                let mut hc = Health::new(argv(&h.cmd));
                hc.interval_s = h.interval_s.unwrap_or(hc.interval_s);
                hc.timeout_s = h.timeout_s.unwrap_or(hc.timeout_s);
                hc.start_period_s = h.start_period_s.unwrap_or(0);
                hc.retries = h.retries.unwrap_or(hc.retries);
                hc
            });
        }
        Instruction::Ignored(kw) => say(format!(" ---> (ignoring {kw}: not supported yet)")),
        Instruction::Workdir(p) => {
            let path = norm(&st.workdir, &dockerfile::subst(p, &st.vars()));
            let k = key(&[&st.chain, "workdir", &path]);
            commit(store, st, k, a.no_cache, say, |stage| {
                let out = stage.join("layer");
                fs::create_dir_all(join_in(&out, &path)).map_err(io_err(format!("mkdir {path}")))?;
                Ok(out)
            })?;
            st.workdir = path;
        }
        Instruction::Copy { srcs, dst, from: from_ref, chown } => {
            let vars = st.vars();
            let srcs: Vec<String> = srcs.iter().map(|s| dockerfile::subst(s, &vars)).collect();
            let dst = dockerfile::subst(dst, &vars);
            let chown = chown.as_ref().map(|c| dockerfile::subst(c, &vars));
            let (source, content) = match from_ref {
                None => (Src::Context(ctx, ig), hash_sources(ctx, ig, &srcs)?),
                Some(f) => {
                    let (layers, id) = copy_source(store, stages, f, say)?;
                    let content = key(&["from", &id, &srcs.join("\u{1}")]);
                    (Src::Layers(layers), content)
                }
            };
            let k = key(&[&st.chain, "copy", &content, &dst, &st.workdir, chown.as_deref().unwrap_or("")]);
            let (lowers, workdir) = (st.layer_paths(store)?, st.workdir.clone());
            commit(store, st, k, a.no_cache, say, |stage| {
                let (out, targets) = copy_layer(stage, &source, &srcs, &dst, &workdir, &lowers)?;
                if let Some(c) = &chown {
                    apply_chown(c, &targets, &lowers)?;
                }
                Ok(out)
            })?;
        }
        Instruction::Run(c) => {
            let argv = argv(c);
            if argv.is_empty() {
                return Err(Error::Usage("RUN needs a command".into()));
            }
            let (lowers, env, workdir, user) = (st.layer_paths(store)?, st.run_env(), st.workdir.clone(), st.user.clone());
            let k = key(&[&st.chain, "run", &argv.join("\u{1}"), &env.join("\u{1}"), &workdir, &user]);
            let text = step.text.clone();
            commit(store, st, k, a.no_cache, say, |stage| {
                let (upper, work, empty) = (stage.join("upper"), stage.join("work"), stage.join("empty"));
                for d in [&upper, &work, &empty] {
                    fs::create_dir_all(d).map_err(io_err(format!("mkdir {}", d.display())))?;
                }
                let lowers = if lowers.is_empty() { vec![empty] } else { lowers }; // FROM scratch
                let code = run_step(&Step { layers: &lowers, upper: &upper, work: &work, cmd: &argv, env: &env, workdir: &workdir, user: &user })?;
                if code != 0 {
                    return Err(Error::Usage(format!("The command '{text}' returned a non-zero code: {code}")));
                }
                scrub(&upper);
                Ok(upper)
            })?;
        }
    }
    Ok(())
}

pub fn build(a: &BuildArgs, say: &mut dyn FnMut(String)) -> Result<ImageMeta> {
    let store = Store::open(registry::data_dir().join("store"));
    let tag = Reference::parse(&a.tag).map_err(img)?;
    let ctx = fs::canonicalize(&a.context).map_err(io_err(format!("build context {}", a.context)))?;
    let df = a.file.as_ref().map_or_else(|| ctx.join("Dockerfile"), PathBuf::from);
    let text = fs::read_to_string(&df).map_err(io_err(format!("read {}", df.display())))?;
    let steps = dockerfile::parse(&text).map_err(img)?;
    let ignore = fs::read_to_string(ctx.join(".dockerignore")).map(|t| Ignore::parse(&t)).unwrap_or_default();

    let total = steps.len();
    let mut globals: HashMap<String, String> = HashMap::new(); // ARGs declared before the first FROM
    let mut stages: Stages = vec![]; // finished stages
    let mut state: Option<State> = None;
    let mut alias: Option<String> = None;
    for (i, step) in steps.iter().enumerate() {
        say(format!("Step {}/{} : {}", i + 1, total, step.text));
        match &step.inst {
            Instruction::Arg { name, default } if state.is_none() && stages.is_empty() => {
                let given = a.args.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
                if let Some(v) = given.or_else(|| default.clone()) {
                    globals.insert(name.clone(), v);
                }
            }
            Instruction::From { image, alias: next_alias } => {
                if let Some(done) = state.take() {
                    let reached = a.target.is_some() && alias == a.target;
                    stages.push((alias.take(), done));
                    if reached {
                        break; // --target: later stages are not built
                    }
                }
                alias = next_alias.clone();
                let image = dockerfile::subst(image, &globals);
                state = Some(match stages.iter().rev().find(|(al, _)| al.as_deref() == Some(image.as_str())) {
                    Some((_, prev)) => State { args: BTreeMap::new(), ..prev.clone() }, // FROM <earlier stage>
                    None => from(&store, &image, say)?,
                });
            }
            _ => {
                let st = state.as_mut().ok_or_else(|| Error::Usage("the Dockerfile must start with FROM".into()))?;
                apply(&store, &ctx, &ignore, a, &globals, &stages, st, step, say)?;
            }
        }
    }
    if let Some(done) = state.take() {
        stages.push((alias.take(), done));
    }
    let st = match &a.target {
        Some(t) => stages.iter().find(|(al, _)| al.as_deref() == Some(t.as_str())).map(|(_, s)| s.clone()).ok_or_else(|| Error::Usage(format!("target stage {t:?} not found")))?,
        None => stages.last().map(|(_, s)| s.clone()).ok_or_else(|| Error::Usage("the Dockerfile has no FROM".into()))?,
    };
    let meta = ImageMeta {
        name: tag.display(),
        layers: st.layers,
        entrypoint: st.entrypoint,
        cmd: st.cmd,
        env: st.env,
        working_dir: if st.workdir == "/" { String::new() } else { st.workdir },
        user: st.user,
        exposed: st.exposed,
        labels: st.labels,
        healthcheck: st.health,
    };
    store.save(&tag, &meta).map_err(img)?;
    say(format!("Successfully built and tagged {}", meta.name));
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rd-build-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn whiteout(p: &Path) -> bool {
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, 0) == 0 }
    }

    #[test]
    fn norm_resolves_relative_and_dotdot() {
        assert_eq!(norm("/", "app"), "/app");
        assert_eq!(norm("/app", "src/"), "/app/src");
        assert_eq!(norm("/app", "../etc/./x"), "/etc/x");
        assert_eq!(norm("/app", "/abs//path/"), "/abs/path");
        assert_eq!(norm("/", ".."), "/");
    }

    #[test]
    fn keys_depend_on_every_part() {
        assert_eq!(key(&["a", "b"]), key(&["a", "b"]));
        assert_ne!(key(&["a", "b"]), key(&["ab", ""]));
        assert_eq!(key(&["x"]).len(), 64);
    }

    #[test]
    fn copy_hash_tracks_content_ignores_and_rejects_escape() {
        let ctx = fs::canonicalize(tmp("hash")).unwrap();
        let none = Ignore::default();
        fs::write(ctx.join("f"), "one").unwrap();
        let a = hash_sources(&ctx, &none, &["f".into()]).unwrap();
        assert_eq!(a, hash_sources(&ctx, &none, &["f".into()]).unwrap());
        fs::write(ctx.join("f"), "two").unwrap();
        assert_ne!(a, hash_sources(&ctx, &none, &["f".into()]).unwrap());

        // an ignored file does not influence the hash, a kept one does
        fs::create_dir_all(ctx.join("d")).unwrap();
        fs::write(ctx.join("d/keep"), "k").unwrap();
        let ig = Ignore::parse("d/skip\n");
        let h1 = hash_sources(&ctx, &ig, &["d".into()]).unwrap();
        fs::write(ctx.join("d/skip"), "noise").unwrap();
        assert_eq!(h1, hash_sources(&ctx, &ig, &["d".into()]).unwrap());
        fs::write(ctx.join("d/keep"), "changed").unwrap();
        assert_ne!(h1, hash_sources(&ctx, &ig, &["d".into()]).unwrap());

        let outside = ctx.parent().unwrap().join(format!("rd-outside-{}", std::process::id()));
        fs::write(&outside, "x").unwrap();
        symlink(&outside, ctx.join("link")).unwrap();
        assert!(resolve_src(&ctx, "link").is_err());
        assert!(resolve_src(&ctx, "../x").is_err());
        assert!(resolve_src(&ctx, "nothing").is_err());
        let _ = fs::remove_file(outside);
        let _ = fs::remove_dir_all(ctx);
    }

    #[test]
    fn copy_places_files_like_docker_and_honours_dockerignore() {
        let ctx = tmp("copy-ctx");
        let stage = tmp("copy-stage");
        fs::create_dir_all(ctx.join("d/inner")).unwrap();
        fs::create_dir_all(ctx.join("d/cache")).unwrap();
        fs::write(ctx.join("d/inner/x"), "x").unwrap();
        fs::write(ctx.join("d/cache/junk"), "j").unwrap();
        fs::write(ctx.join("d/debug.log"), "l").unwrap();
        fs::write(ctx.join("a.txt"), "a").unwrap();
        fs::write(ctx.join("b.txt"), "b").unwrap();
        let ctx = fs::canonicalize(ctx).unwrap();
        let none: Vec<PathBuf> = vec![];
        let ig = Ignore::default();
        let c = Src::Context(&ctx, &ig);

        let (out, _) = copy_layer(&stage, &c, &["a.txt".into()], "/etc/renamed", "/", &none).unwrap();
        assert!(out.join("etc/renamed").is_file());
        let (out, _) = copy_layer(&stage, &c, &["a.txt".into(), "b.txt".into()], "/multi", "/", &none).unwrap();
        assert!(out.join("multi/a.txt").is_file() && out.join("multi/b.txt").is_file());
        let (out, _) = copy_layer(&stage, &c, &["a.txt".into()], "dir/", "/app", &none).unwrap();
        assert!(out.join("app/dir/a.txt").is_file());
        let (out, _) = copy_layer(&stage, &c, &["d".into()], "/dst", "/", &none).unwrap();
        assert!(out.join("dst/inner/x").is_file() && out.join("dst/cache/junk").is_file());

        let _ = fs::remove_dir_all(stage.join("layer")); // a real build gets a fresh staging dir per step
        let ig = Ignore::parse("**/cache\n*.log\n!d/debug.log\n");
        let c = Src::Context(&ctx, &ig);
        let (out, _) = copy_layer(&stage, &c, &["d".into()], "/dst", "/", &none).unwrap();
        assert!(out.join("dst/inner/x").is_file(), "kept files are copied");
        assert!(!out.join("dst/cache").exists(), "ignored directory is skipped");
        assert!(out.join("dst/debug.log").is_file(), "! re-includes a file");
        let ig = Ignore::parse("a.txt\n");
        let err = copy_layer(&stage, &Src::Context(&ctx, &ig), &["a.txt".into()], "/x", "/", &none).unwrap_err();
        assert!(err.to_string().contains(".dockerignore"));

        let lower = tmp("copy-lower");
        fs::create_dir_all(lower.join("app")).unwrap();
        let (out, _) = copy_layer(&stage, &Src::Context(&ctx, &Ignore::default()), &["a.txt".into()], "/app", "/", &[lower.clone()]).unwrap();
        assert!(out.join("app/a.txt").is_file(), "a file copied onto an existing image directory goes inside it");
        for d in [&ctx, &stage, &lower] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn merged_view_applies_whiteouts_shadowing_and_listing() {
        let (l0, l1) = (tmp("m0"), tmp("m1"));
        fs::create_dir_all(l0.join("etc")).unwrap();
        fs::create_dir_all(l0.join("opt/old")).unwrap();
        fs::write(l0.join("etc/motd"), "bottom").unwrap();
        fs::write(l0.join("etc/keep"), "k").unwrap();
        fs::write(l0.join("opt/old/f"), "o").unwrap();
        fs::write(l0.join("shadow"), "low").unwrap();
        fs::create_dir_all(l1.join("etc")).unwrap();
        fs::write(l1.join("etc/new"), "n").unwrap();
        fs::write(l1.join("shadow"), "high").unwrap();
        if !whiteout(&l1.join("etc/motd")) {
            return; // cannot create whiteouts here
        }
        let layers = [l0.clone(), l1.clone()];
        let m = Merged { layers: &layers };
        assert!(m.locate(Path::new("etc/motd")).is_none(), "whited out in the upper layer");
        assert_eq!(fs::read_to_string(m.locate(Path::new("shadow")).unwrap()).unwrap(), "high", "upper layer shadows lower");
        assert!(m.locate(Path::new("etc/keep")).is_some() && m.locate(Path::new("etc/new")).is_some());
        assert!(m.locate(Path::new("opt/old/f")).is_some() && m.locate(Path::new("nope/x")).is_none());
        assert_eq!(m.list(Path::new("etc")).into_iter().collect::<Vec<_>>(), ["keep", "new"]);

        let dst = tmp("m-dst");
        m.copy(Path::new("etc"), &dst.join("etc")).unwrap();
        assert!(dst.join("etc/keep").is_file() && dst.join("etc/new").is_file() && !dst.join("etc/motd").exists());
        assert!(m.copy(Path::new("etc/motd"), &dst.join("x")).is_err());
        for d in [l0, l1, dst] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn scrub_removes_only_runtime_leftovers() {
        let up = tmp("scrub");
        fs::create_dir_all(up.join("etc")).unwrap();
        fs::write(up.join("etc/resolv.conf"), "").unwrap();
        fs::create_dir_all(up.join("proc")).unwrap();
        fs::create_dir_all(up.join("usr/bin")).unwrap();
        fs::write(up.join("usr/bin/tool"), "x").unwrap();
        scrub(&up);
        assert!(!up.join("etc").exists() && !up.join("proc").exists());
        assert!(up.join("usr/bin/tool").is_file());
        fs::create_dir_all(up.join("etc")).unwrap();
        fs::write(up.join("etc/resolv.conf"), "nameserver 1.1.1.1").unwrap(); // real content stays
        scrub(&up);
        assert!(up.join("etc/resolv.conf").is_file());
        let _ = fs::remove_dir_all(up);
    }
}
