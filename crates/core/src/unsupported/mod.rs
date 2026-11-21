//! Platforms without their own container backend (macOS): everything is forwarded to the VM
//! by the CLI before it gets here, so these only exist to keep the shared code compiling.
use crate::{
    cli::{BuildArgs, Config},
    error::{Error, Result},
    registry::Entry,
    stats::Stats,
};

fn needs_vm<T>() -> Result<T> {
    Err(Error::Usage("containers run in the Linux VM here; set it up with `docker vm init`".into()))
}

pub fn run(_: &Config) -> Result<i32> {
    needs_vm()
}

pub fn exec(_: &Entry, _: &[String], _: bool) -> Result<i32> {
    needs_vm()
}

pub fn build(_: &BuildArgs, _: &mut dyn FnMut(String)) -> Result<docker_image::ImageMeta> {
    needs_vm()
}

pub fn stats(_: &str) -> Option<Stats> {
    None
}

pub fn stop(_: &str) -> Result<()> {
    needs_vm()
}

pub fn signal(e: &crate::registry::Entry, _: i32) -> Result<()> {
    stop(&e.id)
}

pub fn update(_: &str, _: Option<u64>, _: Option<u32>, _: Option<u32>) -> Result<()> {
    Err(Error::Usage("update needs the Linux backend".into()))
}

pub fn freeze(_: &str, _: bool) -> Result<()> {
    Err(Error::Usage("pause needs the Linux backend".into()))
}

pub fn top(_: &str) -> Result<Vec<crate::stats::ProcInfo>> {
    Err(Error::Usage("top needs the Linux backend".into()))
}

pub fn cp(_: &crate::registry::Entry, _: crate::stats::CpDir, _: &str, _: &str) -> Result<()> {
    Err(Error::Usage("cp needs the Linux backend".into()))
}
