//! Static catalog of popular Docker Hub images ("what can be added"), with search and Markdown export.

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// exact Docker Hub repository name (the first entry of a name is its default version)
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

pub const CATEGORIES: [&str; 8] = ["Base OS", "Languages", "Web servers & proxies", "Databases", "Messaging", "Apps & tools", "CI & dev", "AI & data"];

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
const AI: &str = CATEGORIES[7];

pub static CATALOG: &[Entry] = &[
    e("ubuntu", "latest", OS, "Ubuntu LTS base image", "", &[], true),
    e("debian", "stable-slim", OS, "Debian stable, slim variant", "", &[], true),
    e("alpine", "latest", OS, "Tiny 5 MB Linux with apk", "", &[], true),
    e("busybox", "latest", OS, "Minimal Unix tools in ~1 MB", "", &[], true),
    e("fedora", "latest", OS, "Fedora base image", "", &[], true),
    e("kalilinux/kali-rolling", "latest", OS, "Kali Linux (security tooling base)", "", &[], true),
    e("archlinux", "latest", OS, "Arch Linux (Intel-only image: runs through x86-64 emulation)", "", &[], true),
    e("node", "alpine", LANG, "Node.js runtime (Alpine)", "3000", &[], true),
    e("python", "alpine", LANG, "Python runtime (Alpine)", "8000", &[], true),
    e("golang", "alpine", LANG, "Go toolchain (Alpine)", "", &[], true),
    e("rust", "alpine", LANG, "Rust toolchain (Alpine)", "", &[], true),
    e("eclipse-temurin", "21-jre-alpine", LANG, "OpenJDK 21 runtime (Temurin)", "8080", &[], true),
    e("php", "cli-alpine", LANG, "PHP command line (Alpine)", "", &[], true),
    e("ruby", "alpine", LANG, "Ruby runtime (Alpine)", "3000", &[], true),
    e("mcr.microsoft.com/dotnet/aspnet", "8.0-alpine", LANG, ".NET 8 ASP.NET runtime (Alpine)", "8080", &[], true),
    e("nginx", "alpine", WEB, "Web server and reverse proxy", "80", &[], true),
    e("httpd", "alpine", WEB, "Apache HTTP Server", "80", &[], true),
    e("caddy", "alpine", WEB, "Web server with automatic HTTPS", "80, 443", &[], true),
    e("traefik", "latest", WEB, "Cloud-native reverse proxy", "80, 8080", &[], true),
    e("haproxy", "alpine", WEB, "TCP/HTTP load balancer", "80", &[], true),
    e("postgres", "alpine", DB, "PostgreSQL database", "5432", &["POSTGRES_PASSWORD=postgres"], true),
    e("mysql", "8", DB, "MySQL database", "3306", &["MYSQL_ROOT_PASSWORD=root"], true),
    e("mariadb", "lts", DB, "MariaDB database (MySQL compatible)", "3306", &["MARIADB_ROOT_PASSWORD=root"], true),
    e("mongo", "latest", DB, "MongoDB document database", "27017", &[], true),
    e("redis", "alpine", DB, "In-memory key-value store", "6379", &[], true),
    e("memcached", "alpine", DB, "Distributed memory cache", "11211", &[], true),
    e("cassandra", "latest", DB, "Wide-column NoSQL database", "9042", &[], true),
    e("couchdb", "latest", DB, "Document database with HTTP API", "5984", &["COUCHDB_USER=admin", "COUCHDB_PASSWORD=admin"], true),
    e("influxdb", "2", DB, "Time-series database", "8086", &[], true),
    e("rabbitmq", "alpine", MSG, "RabbitMQ broker with management UI", "5672, 15672", &[], true),
    e("nats", "alpine", MSG, "NATS messaging server", "4222", &[], true),
    e("redpandadata/redpanda", "latest", MSG, "Kafka-compatible streaming platform", "9092", &[], true),
    e("eclipse-mosquitto", "latest", MSG, "MQTT broker", "1883", &[], true),
    e("wordpress", "apache", APP, "WordPress CMS (needs a MySQL/MariaDB)", "80", &["WORDPRESS_DB_HOST=mariadb", "WORDPRESS_DB_USER=root", "WORDPRESS_DB_PASSWORD=root"], true),
    e("ghost", "alpine", APP, "Ghost blogging platform (SQLite settings below; MySQL for production)", "2368", &["url=http://localhost:2368", "database__client=sqlite3", "database__connection__filename=/var/lib/ghost/content/data/ghost.db"], true),
    e("grafana/grafana", "latest", APP, "Dashboards and visualisation", "3000", &[], true),
    e("prom/prometheus", "latest", APP, "Metrics monitoring", "9090", &[], true),
    e("hashicorp/vault", "latest", APP, "Secrets management", "8200", &[], true),
    e("registry", "2", APP, "Private Docker registry", "5000", &[], true),
    e("mailhog/mailhog", "latest", APP, "Fake SMTP server with web inbox (Intel-only image: runs through x86-64 emulation)", "1025, 8025", &[], true),
    e("adminer", "latest", APP, "Web UI for SQL databases", "8080", &[], true),
    e("jenkins/jenkins", "lts-jdk17", CI, "Jenkins automation server", "8080", &[], true),
    e("gitea/gitea", "latest", CI, "Self-hosted Git service", "3000", &[], true),
    // more versions and images: each tag was checked to exist on Docker Hub, but not run on this engine
    e("ubuntu", "24.04", OS, "Ubuntu 24.04 LTS", "", &[], false),
    e("ubuntu", "22.04", OS, "Ubuntu 22.04 LTS", "", &[], false),
    e("almalinux", "9", OS, "AlmaLinux 9 (RHEL compatible)", "", &[], false),
    e("rockylinux/rockylinux", "9", OS, "Rocky Linux 9 (RHEL compatible)", "", &[], false),
    e("amazonlinux", "latest", OS, "Amazon Linux", "", &[], false),
    e("opensuse/leap", "latest", OS, "openSUSE Leap", "", &[], false),
    e("node", "24-alpine", LANG, "Node.js 24 (Alpine)", "3000", &[], false),
    e("node", "22-alpine", LANG, "Node.js 22 (Alpine)", "3000", &[], false),
    e("node", "22-slim", LANG, "Node.js 22 (Debian slim)", "3000", &[], false),
    e("node", "20-alpine", LANG, "Node.js 20 (Alpine)", "3000", &[], false),
    e("oven/bun", "alpine", LANG, "Bun JavaScript runtime", "3000", &[], false),
    e("denoland/deno", "alpine", LANG, "Deno JavaScript runtime", "8000", &[], false),
    e("python", "3.13-alpine", LANG, "Python 3.13 (Alpine)", "8000", &[], false),
    e("python", "3.13-slim", LANG, "Python 3.13 (Debian slim)", "8000", &[], false),
    e("python", "3.12-slim", LANG, "Python 3.12 (Debian slim)", "8000", &[], false),
    e("python", "3.11-slim", LANG, "Python 3.11 (Debian slim)", "8000", &[], false),
    e("php", "fpm-alpine", LANG, "PHP-FPM (Alpine)", "9000", &[], false),
    e("php", "apache", LANG, "PHP with Apache", "80", &[], false),
    e("composer", "latest", LANG, "PHP dependency manager", "", &[], false),
    e("eclipse-temurin", "25-jdk", LANG, "Java 25 JDK (Temurin)", "", &[], false),
    e("eclipse-temurin", "21-jdk-alpine", LANG, "Java 21 JDK (Alpine)", "", &[], false),
    e("eclipse-temurin", "17-jdk-alpine", LANG, "Java 17 JDK (Intel-only image)", "", &[], false),
    e("eclipse-temurin", "17-jre-alpine", LANG, "Java 17 runtime (Intel-only image)", "", &[], false),
    e("amazoncorretto", "21-alpine", LANG, "Java 21 (Amazon Corretto)", "", &[], false),
    e("maven", "latest", LANG, "Java build tool Maven", "", &[], false),
    e("gradle", "latest", LANG, "Java build tool Gradle", "", &[], false),
    e("golang", "bookworm", LANG, "Go toolchain (Debian)", "", &[], false),
    e("rust", "slim", LANG, "Rust toolchain (Debian slim)", "", &[], false),
    e("ruby", "slim", LANG, "Ruby (Debian slim)", "3000", &[], false),
    e("gcc", "latest", LANG, "C and C++ compiler", "", &[], false),
    e("elixir", "alpine", LANG, "Elixir (Alpine)", "", &[], false),
    e("erlang", "alpine", LANG, "Erlang (Alpine)", "", &[], false),
    e("swift", "latest", LANG, "Swift toolchain", "", &[], false),
    e("dart", "stable", LANG, "Dart SDK", "", &[], false),
    e("perl", "slim", LANG, "Perl", "", &[], false),
    e("julia", "latest", LANG, "Julia", "", &[], false),
    e("haskell", "slim", LANG, "Haskell GHC", "", &[], false),
    e("clojure", "latest", LANG, "Clojure", "", &[], false),
    e("rocker/r-ver", "latest", LANG, "R statistics", "", &[], false),
    e("nginx", "latest", WEB, "Nginx (Debian)", "80", &[], false),
    e("tomcat", "latest", WEB, "Apache Tomcat Java server", "8080", &[], false),
    e("jetty", "latest", WEB, "Jetty Java server", "8080", &[], false),
    e("varnish", "alpine", WEB, "HTTP cache", "80", &[], false),
    e("openresty/openresty", "alpine", WEB, "Nginx with Lua", "80", &[], false),
    e("kong", "latest", WEB, "API gateway", "8000", &[], false),
    e("postgres", "17-alpine", DB, "PostgreSQL 17", "5432", &["POSTGRES_PASSWORD=changeme"], false),
    e("postgres", "16-alpine", DB, "PostgreSQL 16", "5432", &["POSTGRES_PASSWORD=changeme"], false),
    e("pgvector/pgvector", "pg17", DB, "PostgreSQL with vector search", "5432", &["POSTGRES_PASSWORD=changeme"], false),
    e("timescale/timescaledb", "latest-pg17", DB, "Time-series PostgreSQL", "5432", &["POSTGRES_PASSWORD=changeme"], false),
    e("mysql", "lts", DB, "MySQL long-term support", "3306", &["MYSQL_ROOT_PASSWORD=changeme"], false),
    e("redis", "latest", DB, "Redis (Debian)", "6379", &[], false),
    e("valkey/valkey", "alpine", DB, "Open-source Redis fork", "6379", &[], false),
    e("neo4j", "latest", DB, "Graph database", "7474, 7687", &[], false),
    e("clickhouse", "latest", DB, "Analytics column database", "8123", &[], false),
    e("opensearchproject/opensearch", "latest", DB, "Search and analytics engine", "9200", &[], false),
    e("getmeili/meilisearch", "latest", DB, "Fast search engine", "7700", &[], false),
    e("cockroachdb/cockroach", "latest", DB, "Distributed SQL database", "26257", &[], false),
    e("qdrant/qdrant", "latest", DB, "Vector database", "6333", &[], false),
    e("surrealdb/surrealdb", "latest", DB, "Multi-model database", "8000", &[], false),
    e("apache/kafka", "latest", MSG, "Apache Kafka broker", "9092", &[], false),
    e("emqx/emqx", "latest", MSG, "MQTT platform", "1883, 18083", &[], false),
    e("zookeeper", "latest", MSG, "Coordination service", "2181", &[], false),
    e("portainer/portainer-ce", "latest", APP, "Web UI for containers", "9443", &[], false),
    e("n8nio/n8n", "latest", APP, "Workflow automation", "5678", &[], false),
    e("nextcloud", "apache", APP, "Files and collaboration", "80", &[], false),
    e("louislam/uptime-kuma", "1", APP, "Uptime monitoring", "3001", &[], false),
    e("phpmyadmin", "latest", APP, "Web UI for MySQL", "80", &[], false),
    e("dpage/pgadmin4", "latest", APP, "Web UI for PostgreSQL", "80", &["PGADMIN_DEFAULT_EMAIL=admin@example.com", "PGADMIN_DEFAULT_PASSWORD=changeme"], false),
    e("mongo-express", "latest", APP, "Web UI for MongoDB", "8081", &[], false),
    e("metabase/metabase", "latest", APP, "Business dashboards", "3000", &[], false),
    e("vaultwarden/server", "latest", APP, "Password manager server", "80", &[], false),
    e("directus/directus", "latest", APP, "Headless CMS", "8055", &[], false),
    e("drupal", "latest", APP, "Drupal CMS", "80", &[], false),
    e("joomla", "latest", APP, "Joomla CMS", "80", &[], false),
    e("redmine", "latest", APP, "Project tracker", "3000", &[], false),
    e("matomo", "latest", APP, "Web analytics", "80", &[], false),
    e("sonarqube", "community", CI, "Code quality scanner", "9000", &[], false),
    e("gitlab/gitlab-ce", "latest", CI, "GitLab (large, needs several GB of RAM)", "80", &[], false),
    e("docker", "cli", CI, "Docker command line", "", &[], false),
    e("hashicorp/terraform", "latest", CI, "Infrastructure as code", "", &[], false),
    e("selenium/standalone-chromium", "latest", CI, "Browser testing", "4444", &[], false),
    e("ollama/ollama", "latest", AI, "Run LLMs locally", "11434", &[], false),
    e("chromadb/chroma", "latest", AI, "Vector store for AI apps", "8000", &[], false),
    e("apache/spark", "latest", AI, "Data processing engine", "", &[], false),
    e("apache/airflow", "latest", AI, "Workflow scheduler", "8080", &[], false),
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

/// A `docker search` result.
pub struct Hit {
    pub name: String,
    pub desc: String,
    pub official: bool,
}

/// Parses the `docker search` table (name in 36 columns, then description in 50).
pub fn parse_search(output: &str) -> Vec<Hit> {
    output
        .lines()
        .skip(1)
        .filter_map(|l| {
            let name = l.split_whitespace().next()?.to_string();
            let desc = l.chars().skip(37).take(50).collect::<String>().trim().to_string();
            Some(Hit { name, desc, official: l.trim_end().ends_with("[OK]") })
        })
        .collect()
}

/// Parses `docker catalog`: one `image<TAB>description` per line.
pub fn parse_mine(output: &str) -> Vec<(String, String)> {
    output.lines().filter(|l| !l.trim().is_empty()).map(|l| l.split_once('\t').unwrap_or((l, ""))).map(|(i, d)| (i.trim().to_string(), d.trim().to_string())).collect()
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
    s.push_str(&format!("{tested} of {} images were actually pulled and run on this engine (**tested**). Any other Docker Hub image can be pulled by name too (Intel-only images run through x86-64 emulation); those have not been tried.\n", CATALOG.len()));
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
        let names: HashSet<_> = CATALOG.iter().map(Entry::reference).collect();
        assert_eq!(names.len(), CATALOG.len());
        assert!(CATALOG.len() >= 120);
        assert!(CATALOG.iter().all(|c| CATEGORIES.contains(&c.category) && !c.tag.is_empty() && !c.desc.is_empty()));
        for k in CATEGORIES {
            assert!(CATALOG.iter().any(|c| c.category == k), "empty category {k}");
        }
    }

    #[test]
    fn tested_means_run_on_this_engine() {
        // only these were pulled and run here (see scripts/live-images.sh)
        assert_eq!(CATALOG.iter().filter(|c| c.tested).count(), 43);
        assert!(!CATALOG.iter().any(|c| c.name == "minio/minio"), "minio/minio is not pullable any more");
        let t: Vec<String> = CATALOG.iter().map(Entry::reference).collect();
        for must in ["alpine:latest", "postgres:alpine", "mongo:latest", "kalilinux/kali-rolling:latest", "archlinux:latest", "jenkins/jenkins:lts-jdk17"] {
            assert!(t.iter().any(|r| r == must), "{must}");
        }
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
    fn parses_cli_output() {
        let row = |n: &str, d: &str, o: &str| format!("{n:<36} {d:<50} {:>6} {o:>8}\n", 5);
        let h = parse_search(&[row("NAME", "DESCRIPTION", "OFFICIAL"), row("nginx", "Web server", "[OK]"), row("u/n", "", "")].concat());
        assert_eq!((h[0].name.as_str(), h[0].desc.as_str(), h[0].official), ("nginx", "Web server", true));
        assert_eq!((h.len(), h[1].name.as_str(), h[1].official), (2, "u/n", false));
        assert_eq!(parse_mine("a:1\tmine\nb\t\n\n"), [("a:1".to_string(), "mine".to_string()), ("b".to_string(), String::new())]);
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
        assert!(m.contains("| `ubuntu:latest` |") && m.contains("| `node:22-alpine` | Node.js 22 (Alpine) | 3000 | untested |"));
        assert!(m.contains(&format!("43 of {} images", CATALOG.len())));
    }
}
