//! `docker catalog`: your own list of images, shown in the app's "Add image" dialog.
//! It lives on this computer (not inside the Linux VM), one `image<TAB>description` per line.
use std::{error::Error, fs, path::PathBuf};

const USAGE: &str = "usage: docker catalog [ls] | add <image> [description] | rm <image>";

fn file() -> Result<PathBuf, Box<dyn Error>> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok_or("no home directory")?;
    Ok(PathBuf::from(home).join(".sasta-docker").join("images.txt"))
}

/// The new file content after `add` or `rm`.
fn edit(text: &str, args: &[String]) -> Result<String, String> {
    let (verb, image, desc) = match args {
        [v, i, d @ ..] if v == "add" => (v, i, d.join(" ")),
        [v, i] if v == "rm" => (v, i, String::new()),
        _ => return Err(USAGE.into()),
    };
    if image.is_empty() || image.starts_with('-') || image.contains(char::is_whitespace) {
        return Err(format!("{image:?} is not an image name"));
    }
    let mut lines: Vec<String> = text.lines().filter(|l| l.split('\t').next() != Some(image)).map(String::from).collect();
    if verb == "add" {
        lines.push(format!("{image}\t{}", desc.replace(['\t', '\n', '\r'], " ")));
    } else if lines.len() == text.lines().count() {
        return Err(format!("{image} is not in your list"));
    }
    Ok(lines.iter().map(|l| format!("{l}\n")).collect())
}

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let path = file()?;
    let text = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        r => r?,
    };
    if args.is_empty() || args == ["ls"] {
        print!("{text}");
        return Ok(());
    }
    let new = edit(&text, args)?;
    fs::create_dir_all(path.parent().ok_or("no home directory")?)?;
    fs::write(&path, new)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn add_replace_remove() {
        let t = edit("", &a(&["add", "ghcr.io/o/a:1", "my", "app"])).unwrap();
        assert_eq!(t, "ghcr.io/o/a:1\tmy app\n");
        let t = edit(&t, &a(&["add", "redis:7"])).unwrap();
        let t = edit(&t, &a(&["add", "ghcr.io/o/a:1", "new"])).unwrap();
        assert_eq!(t, "redis:7\t\nghcr.io/o/a:1\tnew\n");
        assert_eq!(edit(&t, &a(&["rm", "redis:7"])).unwrap(), "ghcr.io/o/a:1\tnew\n");
        assert!(edit(&t, &a(&["rm", "nope"])).is_err());
        assert!(edit(&t, &a(&["add", "a b"])).is_err());
        assert!(edit(&t, &a(&["add"])).is_err());
    }
}
