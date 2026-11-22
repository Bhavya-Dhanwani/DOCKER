//! Static catalog of popular Docker Hub images ("what can be added"), with search and Markdown export.

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// exact Docker Hub repository name
    pub name: &'static str,
    pub desc: &'static str,
    pub category: &'static str,
    pub tag: &'static str,
    /// typical published port(s), informational
    pub ports: &'static str,
    /// env vars the image needs to start, as KEY=suggested-value
    pub env: &'static [&'static str],
    /// actually run on this engine
    pub tested: bool,
}

impl Entry {
    pub fn reference(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }
}

pub const CATEGORIES: [&str; 7] = ["Base OS", "Languages", "Web servers & proxies", "Databases", "Messaging", "Apps & tools", "CI & dev"];

const fn e(name: &'static str, tag: &'static str, category: &'static str, desc: &'static str, ports: &'static str, env: &'static [&'static str], tested: bool) -> Entry {
    Entry { name, desc, category, tag, ports, env, tested }
}

const OS: &str = CATEGORIES[0];
const LANG: &str = CATEGORIES[1];
const WEB: &str = CATEGORIES[2];
const DB: &str = CATEGORIES[3];
const MSG: &str = CATEGORIES[4];
const APP: &str = CATEGORIES[5];
const CI: &str = CATEGORIES[6];

pub static CATALOG: &[Entry] = &[
    e("ubuntu", "latest", OS, "Ubuntu LTS base image", "", &[], false),
    e("debian", "stable-slim", OS, "Debian stable, slim variant", "", &[], false),
    e("alpine", "latest", OS, "Tiny 5 MB Linux with apk", "", &[], true),
    e("busybox", "latest", OS, "Minimal Unix tools in ~1 MB", "", &[], false),
    e("fedora", "latest", OS, "Fedora base image", "", &[], false),
    e("node", "alpine", LANG, "Node.js runtime (Alpine)", "3000", &[], true),
    e("python", "alpine", LANG, "Python runtime (Alpine)", "8000", &[], true),
    e("golang", "alpine", LANG, "Go toolchain (Alpine)", "", &[], false),
    e("rust", "alpine", LANG, "Rust toolchain (Alpine)", "", &[], false),
    e("eclipse-temurin", "21-jre-alpine", LANG, "OpenJDK 21 runtime (Temurin)", "8080", &[], false),
    e("php", "cli-alpine", LANG, "PHP command line (Alpine)", "", &[], false),
    e("ruby", "alpine", LANG, "Ruby runtime (Alpine)", "3000", &[], false),
    e("mcr.microsoft.com/dotnet/aspnet", "8.0-alpine", LANG, ".NET 8 ASP.NET runtime (Alpine)", "8080", &[], false),
    e("nginx", "alpine", WEB, "Web server and reverse proxy", "80", &[], true),
    e("httpd", "alpine", WEB, "Apache HTTP Server", "80", &[], false),
    e("caddy", "alpine", WEB, "Web server with automatic HTTPS", "80, 443", &[], false),
    e("traefik", "latest", WEB, "Cloud-native reverse proxy", "80, 8080", &[], false),
    e("haproxy", "alpine", WEB, "TCP/HTTP load balancer", "80", &[], false),
    e("postgres", "alpine", DB, "PostgreSQL database", "5432", &["POSTGRES_PASSWORD=postgres"], true),
    e("mysql", "lts", DB, "MySQL database", "3306", &["MYSQL_ROOT_PASSWORD=root"], false),
    e("mariadb", "lts", DB, "MariaDB database (MySQL compatible)", "3306", &["MARIADB_ROOT_PASSWORD=root"], true),
    e("mongo", "latest", DB, "MongoDB document database", "27017", &[], false),
    e("redis", "alpine", DB, "In-memory key-value store", "6379", &[], true),
    e("memcached", "alpine", DB, "Distributed memory cache", "11211", &[], false),
    e("cassandra", "latest", DB, "Wide-column NoSQL database", "9042", &[], false),
    e("couchdb", "latest", DB, "Document database with HTTP API", "5984", &["COUCHDB_USER=admin", "COUCHDB_PASSWORD=admin"], false),
    e("influxdb", "2", DB, "Time-series database", "8086", &[], false),
    e("rabbitmq", "4-management-alpine", MSG, "RabbitMQ broker with management UI", "5672, 15672", &[], false),
    e("nats", "alpine", MSG, "NATS messaging server", "4222", &[], false),
    e("redpandadata/redpanda", "latest", MSG, "Kafka-compatible streaming platform", "9092", &[], false),
    e("eclipse-mosquitto", "latest", MSG, "MQTT broker", "1883", &[], false),
    e("wordpress", "latest", APP, "WordPress CMS (needs a MySQL/MariaDB)", "80", &["WORDPRESS_DB_HOST=mariadb", "WORDPRESS_DB_USER=root", "WORDPRESS_DB_PASSWORD=root"], false),
    e("ghost", "alpine", APP, "Ghost blogging platform", "2368", &[], false),
    e("grafana/grafana", "latest", APP, "Dashboards and visualisation", "3000", &[], false),
    e("prom/prometheus", "latest", APP, "Metrics monitoring", "9090", &[], false),
    e("hashicorp/vault", "latest", APP, "Secrets management", "8200", &[], false),
    e("registry", "2", APP, "Private Docker registry", "5000", &[], false),
    e("minio/minio", "latest", APP, "S3-compatible object storage", "9000, 9001", &["MINIO_ROOT_USER=minioadmin", "MINIO_ROOT_PASSWORD=minioadmin"], false),
    e("mailhog/mailhog", "latest", APP, "Fake SMTP server with web inbox", "1025, 8025", &[], false),
    e("adminer", "latest", APP, "Web UI for SQL databases", "8080", &[], false),
    e("jenkins/jenkins", "lts", CI, "Jenkins automation server", "8080", &[], false),
    e("gitea/gitea", "latest", CI, "Self-hosted Git service", "3000", &[], false),
];

/// The short name of an image reference: `docker.io/library/postgres:alpine` -> `postgres`.
pub fn short_name(image: &str) -> String {
    let no_digest = image.split('@').next().unwrap_or(image);
    let leaf = no_digest.rsplit('/').next().unwrap_or(no_digest);
    leaf.split(':').next().unwrap_or(leaf).to_string()
}

/// Catalog entry for an image reference, matching on the repository name (tag ignored).
pub fn lookup(image: &str) -> Option<&'static Entry> {
    let repo = image.split('@').next().unwrap_or(image).rsplit_once(':').filter(|(_, t)| !t.contains('/')).map_or(image, |(r, _)| r);
    let repo = repo.strip_prefix("docker.io/").unwrap_or(repo);
    let repo = repo.strip_prefix("library/").unwrap_or(repo);
    CATALOG.iter().find(|c| c.name == repo)
}

/// Entries matching a free-text query (name, description, category) and optionally one category.
pub fn search(query: &str, category: Option<&str>) -> Vec<&'static Entry> {
    let q = query.trim().to_lowercase();
    CATALOG
        .iter()
        .filter(|c| category.is_none_or(|k| c.category == k))
        .filter(|c| q.is_empty() || [c.name, c.desc, c.category].iter().any(|f| f.to_lowercase().contains(&q)))
        .collect()
}

/// `docker pull` arguments; an image name typed by the user is used as is, a bare name is not changed.
pub fn pull_args(reference: &str) -> Result<Vec<String>, String> {
    let r = reference.trim();
    if r.is_empty() || r.contains(char::is_whitespace) {
        return Err("Image name must not be empty or contain spaces".into());
    }
    Ok(vec!["pull".into(), r.into()])
}

pub fn to_markdown() -> String {
    let mut s = String::from("# Sasta Docker: image catalog\n\n");
    let tested = CATALOG.iter().filter(|c| c.tested).count();
    s.push_str(&format!("{tested} of {} images were actually run on this engine (**tested**); the rest are expected to work but are **untested**.\n", CATALOG.len()));
    for cat in CATEGORIES {
        s.push_str(&format!("\n## {cat}\n\n| Image | Description | Port(s) | Status |\n|---|---|---|---|\n"));
        for c in CATALOG.iter().filter(|c| c.category == cat) {
            let ports = if c.ports.is_empty() { "-" } else { c.ports };
            s.push_str(&format!("| `{}` | {} | {ports} | {} |\n", c.reference(), c.desc, if c.tested { "tested" } else { "untested" }));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn no_duplicates_and_known_categories() {
        let names: HashSet<_> = CATALOG.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), CATALOG.len());
        assert!(CATALOG.len() >= 40);
        assert!(CATALOG.iter().all(|c| CATEGORIES.contains(&c.category) && !c.tag.is_empty() && !c.desc.is_empty()));
        for k in CATEGORIES {
            assert!(CATALOG.iter().any(|c| c.category == k), "empty category {k}");
        }
    }

    #[test]
    fn tested_set_is_exactly_the_seven() {
        let mut t: Vec<String> = CATALOG.iter().filter(|c| c.tested).map(Entry::reference).collect();
        t.sort();
        assert_eq!(t, ["alpine:latest", "mariadb:lts", "nginx:alpine", "node:alpine", "postgres:alpine", "python:alpine", "redis:alpine"]);
    }

    #[test]
    fn search_and_filter() {
        assert_eq!(search("", None).len(), CATALOG.len());
        assert_eq!(search("POSTGRES", None)[0].name, "postgres");
        assert!(search("zzz-nothing", None).is_empty());
        assert!(search("", Some("Databases")).iter().all(|c| c.category == "Databases"));
        assert!(search("redis", Some("Base OS")).is_empty());
        assert!(search("kafka", None).iter().any(|c| c.name == "redpandadata/redpanda"));
    }

    #[test]
    fn names_and_lookup() {
        assert_eq!(short_name("docker.io/library/postgres:alpine"), "postgres");
        assert_eq!(short_name("localhost:5000/team/app:1"), "app");
        assert_eq!(short_name("redis"), "redis");
        assert_eq!(lookup("postgres:alpine").unwrap().name, "postgres");
        assert_eq!(lookup("grafana/grafana:10").unwrap().name, "grafana/grafana");
        assert_eq!(lookup("docker.io/library/redis").unwrap().name, "redis");
        assert!(lookup("unknown/thing").is_none());
    }

    #[test]
    fn pull_arg_building() {
        assert_eq!(pull_args(" redis:7 ").unwrap(), ["pull", "redis:7"]);
        assert!(pull_args("").is_err());
        assert!(pull_args("a b").is_err());
    }

    #[test]
    fn markdown_lists_status() {
        let m = to_markdown();
        assert!(m.contains("| `postgres:alpine` | PostgreSQL database | 5432 | tested |"));
        assert!(m.contains("| `ubuntu:latest` |") && m.contains("untested"));
        assert!(m.contains("7 of "));
    }
}
