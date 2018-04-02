extern crate buildchain;
extern crate dbus;
extern crate ecflash;
extern crate libc;
extern crate lzma;
extern crate plain;
extern crate serde_json;
extern crate sha2;
extern crate tar;
extern crate tempdir;
extern crate uuid;

use buildchain::{Downloader, Manifest};
use dbus::{Connection, BusType, NameFlag};
use dbus::tree::{Factory, MethodErr};
use std::{fs, io, process};
use std::path::Path;

mod bios;
mod boot;
mod config;
mod download;
mod ec;
mod me;
mod mount;
mod util;

// Helper function for errors
pub (crate) fn err_str<E: ::std::fmt::Display>(err: E) -> String {
    format!("{}", err)
}

fn firmware_id() -> Result<String, String> {
    let (bios_model, _bios_version) = bios::bios()?;
    let (ec_project, _ec_version) = ec::ec_or_none(true);
    let ec_hash = util::sha256(ec_project.as_bytes());
    Ok(format!("{}_{}", bios_model, ec_hash))
}

fn remove_dir<P: AsRef<Path>>(path: P) -> Result<(), String> {
    if path.as_ref().exists() {
        eprintln!("removing {}", path.as_ref().display());
        match fs::remove_dir_all(&path) {
            Ok(()) => (),
            Err(err) => {
                return Err(format!("failed to remove {}: {}", path.as_ref().display(), err));
            }
        }
    }

    Ok(())
}

fn download() -> Result<(String, String), String> {
    let firmware_id = firmware_id()?;

    let dl = Downloader::new(
        config::KEY,
        config::URL,
        config::PROJECT,
        config::BRANCH,
        Some(config::CERT)
    )?;

    let tail = dl.tail()?;

    let cache = download::Cache::new(config::CACHE, Some(dl))?;

    eprintln!("downloading manifest.json");
    let manifest_json = cache.object(&tail.digest)?;
    let manifest = serde_json::from_slice::<Manifest>(&manifest_json).map_err(|e| e.to_string())?;

    let _updater_data = {
        let file = "system76-firmware-update.tar.xz";
        eprintln!("downloading {}", file);
        let digest = manifest.files.get(file).ok_or(format!("{} not found", file))?;
        cache.object(&digest)?
    };

    let firmware_data = {
        let file = format!("{}.tar.xz", firmware_id);
        eprintln!("downloading {}", file);
        let digest = manifest.files.get(&file).ok_or(format!("{} not found", file))?;
        cache.object(&digest)?
    };

    let changelog = util::extract_file(&firmware_data, "./changelog.json").map_err(err_str)?;

    Ok((tail.digest.to_string(), changelog))
}

fn extract<P: AsRef<Path>>(digest: &str, file: &str, path: P) -> Result<(), String> {
    let cache = download::Cache::new(config::CACHE, None)?;

    let manifest_json = cache.object(&digest)?;
    let manifest = serde_json::from_slice::<Manifest>(&manifest_json).map_err(|e| e.to_string())?;

    let data = {
        let digest = manifest.files.get(file).ok_or(format!("{} not found", file))?;
        cache.object(&digest)?
    };

    eprintln!("extracting {} to {}", file, path.as_ref().display());
    match util::extract(&data, &path) {
        Ok(()) => (),
        Err(err) => {
            return Err(format!("failed to extract {} to {}: {}", file, path.as_ref().display(), err));
        }
    }

    Ok(())
}

fn schedule(digest: &str) -> Result<(), String> {
    let firmware_id = firmware_id()?;

    if ! Path::new("/sys/firmware/efi").exists() {
        return Err(format!("must be run using UEFI boot"));
    }

    let updater_file = "system76-firmware-update.tar.xz";
    let firmware_file = format!("{}.tar.xz", firmware_id);
    let updater_dir = Path::new("/boot/efi/system76-firmware-update");

    boot::unset_next_boot()?;

    remove_dir(&updater_dir)?;

    let updater_tmp = match tempdir::TempDir::new_in("/boot/efi", "system76-firmware-update") {
        Ok(ok) => ok,
        Err(err) => {
            return Err(format!("failed to create temporary directory: {}", err));
        }
    };

    extract(digest, updater_file, updater_tmp.path())?;

    extract(digest, &firmware_file, &updater_tmp.path().join("firmware"))?;

    let updater_tmp_dir = updater_tmp.into_path();
    eprintln!("moving {} to {}", updater_tmp_dir.display(), updater_dir.display());
    match fs::rename(&updater_tmp_dir, &updater_dir) {
        Ok(()) => (),
        Err(err) => {
            let _ = remove_dir(&updater_tmp_dir);
            return Err(format!("failed to move {} to {}: {}", updater_tmp_dir.display(), updater_dir.display(), err));
        }
    }

    boot::set_next_boot()?;

    eprintln!("Firmware update scheduled. Reboot your machine to install.");

    Ok(())
}

fn unschedule() -> Result<(), String> {
    let updater_dir = Path::new("/boot/efi/system76-firmware-update");

    boot::unset_next_boot()?;

    remove_dir(&updater_dir)?;

    eprintln!("Firmware update cancelled.");

    Ok(())
}

pub fn bus() -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(format!("must be run as root"));
    }

    // Get I/O Permission
    if unsafe { libc::iopl(3) } < 0 {
        return Err(format!(
            "failed to get I/O permission: {}",
            io::Error::last_os_error()
        ));
    }

    let c = Connection::get_private(BusType::System).map_err(err_str)?;
    c.register_name("com.system76.FirmwareDaemon", NameFlag::ReplaceExisting as u32).map_err(err_str)?;

    let f = Factory::new_fn::<()>();

    let tree = f.tree(()).add(f.object_path("/com/system76/FirmwareDaemon", ()).introspectable().add(
        f.interface("com.system76.FirmwareDaemon", ())
        .add_m(
            f.method("Bios", (), move |m| {
                eprintln!("Bios");
                match bios::bios() {
                    Ok((bios_model, bios_version)) => {
                        let mret = m.msg.method_return().append2(bios_model, bios_version);
                        Ok(vec![mret])
                    },
                    Err(err) => {
                        eprintln!("{}", err);
                        Err(MethodErr::failed(&err))
                    }
                }
            })
            .outarg::<&str,_>("model")
            .outarg::<&str,_>("version")
        )
        .add_m(
            f.method("EmbeddedController", (), move |m| {
                let primary = m.msg.read1()?;
                eprintln!("EmbeddedController({})", primary);
                match ec::ec(primary) {
                    Ok((ec_project, ec_version)) => {
                        let mret = m.msg.method_return().append2(ec_project, ec_version);
                        Ok(vec![mret])
                    },
                    Err(err) => {
                        eprintln!("{}", err);
                        Err(MethodErr::failed(&err))
                    }
                }
            })
            .inarg::<bool,_>("primary")
            .outarg::<&str,_>("project")
            .outarg::<&str,_>("version")
        )
        .add_m(
            f.method("ManagementEngine", (), move |m| {
                eprintln!("ManagementEngine");
                match me::me() {
                    Ok(Some(me_version)) => {
                        let mret = m.msg.method_return().append2(true, me_version);
                        Ok(vec![mret])
                    },
                    Ok(None) => {
                        let mret = m.msg.method_return().append2(false, "");
                        Ok(vec![mret])
                    },
                    Err(err) => {
                        eprintln!("{}", err);
                        Err(MethodErr::failed(&err))
                    }
                }
            })
            .outarg::<bool,_>("enabled")
            .outarg::<&str,_>("version")
        )
        .add_m(
            f.method("FirmwareId", (), move |m| {
                eprintln!("FirmwareId");
                match firmware_id() {
                    Ok(id) => {
                        let mret = m.msg.method_return().append1(id);
                        Ok(vec![mret])
                    },
                    Err(err) => {
                        eprintln!("{}", err);
                        Err(MethodErr::failed(&err))
                    }
                }
            })
            .outarg::<&str,_>("id")
        )
        .add_m(
            f.method("Download", (), move |m| {
                eprintln!("Download");
                match download() {
                    Ok((digest, changelog)) => {
                        let mret = m.msg.method_return().append2(digest, changelog);
                        Ok(vec![mret])
                    },
                    Err(err) => {
                        eprintln!("{}", err);
                        Err(MethodErr::failed(&err))
                    }
                }
            })
            .outarg::<&str,_>("digest")
            .outarg::<&str,_>("changelog")
        )
        .add_m(
            f.method("Schedule", (), move |m| {
                let digest = m.msg.read1()?;
                eprintln!("Schedule({})", digest);
                match schedule(digest) {
                    Ok(()) => {
                        let mret = m.msg.method_return();
                        Ok(vec![mret])
                    },
                    Err(err) => {
                        eprintln!("{}", err);
                        Err(MethodErr::failed(&err))
                    }
                }
            })
            .inarg::<&str,_>("digest")
        )
        .add_m(
            f.method("Unschedule", (), move |m| {
                eprintln!("Unschedule");
                match unschedule() {
                    Ok(()) => {
                        let mret = m.msg.method_return();
                        Ok(vec![mret])
                    },
                    Err(err) => {
                        eprintln!("{}", err);
                        Err(MethodErr::failed(&err))
                    }
                }
            })
        )
    ));

    tree.set_registered(&c, true).map_err(err_str)?;

    c.add_handler(tree);

    loop {
        c.incoming(1000).next();
    }
}

fn main() {
    match bus() {
        Ok(()) => (),
        Err(err) => {
            eprintln!("system76-firmware-daemon: {}", err);
            process::exit(1);
        }
    }
}
