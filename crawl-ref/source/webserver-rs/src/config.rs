//! Typed configuration, mirroring `webserver/webtiles/config.py` and
//! `webserver/config.py`. See `../ARCHITECTURE.md` §6 for the layering
//! rules this module implements.
//!
//! Load order (later wins, except `games`/`banned`, see [`ServerConfig::merge_overrides`]):
//! 1. [`ServerConfig::default`] (built-in defaults, from `webtiles.config.defaults`)
//! 2. `config.yml` (if present)
//! 3. `games.d/*.yml` (games/templates only, unless `games` was already set in `config.yml`)
//! 4. CLI overrides ([`CliOverrides`])

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, WebtilesError};

/// Top-level server configuration. Field defaults match
/// `webtiles.config.defaults` in the Python implementation exactly, so an
/// operator migrating an existing deployment gets the same behavior with an
/// empty `config.yml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub dgl_mode: bool,
    pub bind_nonsecure: BindMode,
    pub bind_address: String,
    pub bind_port: u16,
    pub bind_pairs: Vec<(String, u16)>,

    pub ssl_address: String,
    pub ssl_port: u16,
    pub ssl_bind_pairs: Vec<(String, u16)>,
    pub ssl_cert_file: Option<PathBuf>,
    pub ssl_key_file: Option<PathBuf>,

    pub password_db: PathBuf,
    pub settings_db: PathBuf,

    pub static_path: PathBuf,
    pub template_path: PathBuf,

    pub server_socket_path: Option<PathBuf>,
    pub server_id: String,

    pub game_data_no_cache: bool,
    pub watch_socket_dirs: bool,

    pub max_connections: u32,
    pub connection_timeout_secs: u64,
    pub max_idle_time_secs: u64,
    pub max_lobby_idle_time_secs: u64,
    pub kill_timeout_secs: u64,

    pub nick_regex: String,
    pub max_passwd_length: usize,
    pub allow_password_reset: bool,
    pub admin_password_reset: bool,
    pub crypt_algorithm: CryptAlgorithm,
    pub crypt_salt_length: usize,
    pub login_token_lifetime_days: i64,
    pub recovery_token_lifetime_hours: i64,

    pub lobby_update_rate_secs: u64,
    pub status_file_update_rate_secs: u64,
    pub dgl_status_file: Option<PathBuf>,

    pub use_gzip: bool,
    pub no_cache: bool,
    pub development_mode: bool,
    pub live_debug: bool,

    pub enable_ttyrecs: bool,
    pub recording_term_size: (u16, u16),

    pub allow_anon_spectate: bool,
    pub max_chat_length: usize,
    pub new_accounts_disabled: bool,
    pub new_accounts_hold: bool,
    pub bot_accounts: bool,
    pub wizard_accounts: bool,

    pub banned: Vec<String>,
    pub autologin: Option<String>,

    pub games: BTreeMap<String, GameConfig>,
    pub templates: BTreeMap<String, GameTemplate>,
    pub use_game_yaml: Option<bool>,

    pub daemon: bool,
    pub pidfile: Option<PathBuf>,
    pub umask: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub chroot: Option<PathBuf>,

    pub lobby_url: Option<String>,
    pub player_url: Option<String>,

    /// Populated by [`ServerConfig::load`]; the directory containing the
    /// primary config file. Equivalent to Python's `config.server_path`.
    #[serde(skip)]
    pub server_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindMode {
    Disabled,
    Enabled,
    Redirect,
}

impl Default for BindMode {
    fn default() -> Self {
        BindMode::Enabled
    }
}

/// Legacy password hashing scheme selector, matching `crypt_algorithm` in
/// the Python config. **Not used to select the hashing scheme for new/
/// changed passwords** in this implementation — see `src/auth.rs`, which
/// always uses Argon2id for that and only consults a stored hash's own
/// prefix (not this setting) to verify legacy hashes. Retained purely so
/// an existing `config.yml` carried over from a Python deployment still
/// parses without error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CryptAlgorithm {
    Broken,
    /// glibc crypt(3) id string, e.g. "5" (SHA-256) or "6" (SHA-512).
    Id(u8),
}

impl Default for CryptAlgorithm {
    fn default() -> Self {
        CryptAlgorithm::Broken
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            dgl_mode: true,
            bind_nonsecure: BindMode::Enabled,
            bind_address: String::new(),
            bind_port: 8080,
            bind_pairs: Vec::new(),

            ssl_address: String::new(),
            ssl_port: 8443,
            ssl_bind_pairs: Vec::new(),
            ssl_cert_file: None,
            ssl_key_file: None,

            password_db: PathBuf::from("./webserver/passwd.db3"),
            settings_db: PathBuf::from("./webserver/user_settings.db3"),

            static_path: PathBuf::from("./webserver/static"),
            template_path: PathBuf::from("./webserver/templates/"),

            server_socket_path: None,
            server_id: String::new(),

            game_data_no_cache: true,
            watch_socket_dirs: false,

            max_connections: 100,
            connection_timeout_secs: 10 * 60,
            max_idle_time_secs: 5 * 60 * 60,
            max_lobby_idle_time_secs: 3 * 60 * 60,
            kill_timeout_secs: 10,

            nick_regex: r"^[a-zA-Z0-9]{3,20}$".to_string(),
            max_passwd_length: 20,
            allow_password_reset: false,
            admin_password_reset: false,
            crypt_algorithm: CryptAlgorithm::Broken,
            crypt_salt_length: 16,
            login_token_lifetime_days: 7,
            recovery_token_lifetime_hours: 12,

            lobby_update_rate_secs: 2,
            status_file_update_rate_secs: 30,
            dgl_status_file: None,

            use_gzip: true,
            no_cache: false,
            development_mode: false,
            live_debug: false,

            enable_ttyrecs: true,
            recording_term_size: (80, 24),

            allow_anon_spectate: true,
            max_chat_length: 1000,
            new_accounts_disabled: false,
            new_accounts_hold: false,
            bot_accounts: false,
            wizard_accounts: false,

            banned: Vec::new(),
            autologin: None,

            games: BTreeMap::new(),
            templates: BTreeMap::new(),
            use_game_yaml: None,

            daemon: false,
            pidfile: None,
            umask: None,
            uid: None,
            gid: None,
            chroot: None,

            lobby_url: None,
            player_url: None,

            server_path: PathBuf::new(),
        }
    }
}

/// A game template: shared defaults inherited by [`GameConfig`] entries via
/// `template: <name>`. Same field surface as a game (Python's `GameConfig`
/// is used interchangeably for both), kept as a distinct type in Rust for
/// clarity at call sites.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GameTemplate {
    /// Only present (and used) for `games.d/*.yml` list entries, which are
    /// keyed by an inline `id:` field (see `base.yaml`); the `config.yml`
    /// map form keys templates by the map key instead and typically omits
    /// this field entirely.
    pub id: String,
    /// A template may itself inherit from another template (e.g. `trunk`
    /// inheriting from `default`), same as a game.
    pub template: Option<String>,
    #[serde(flatten)]
    pub fields: GameFields,
}

/// One playable game/mode entry (`games.d/*.yml` `games:` list, or the
/// `games` map in `config.yml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GameConfig {
    pub id: String,
    pub template: Option<String>,
    #[serde(flatten)]
    pub fields: GameFields,
}

/// Fields shared between templates and concrete games. Optional so that a
/// game can omit a field and fall back to its template (see
/// [`GameConfig::resolve`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GameFields {
    pub name: Option<String>,
    pub version: Option<String>,
    pub crawl_binary: Option<PathBuf>,
    pub pre_options: Vec<String>,
    pub options: Vec<String>,
    pub rcfile_path: Option<String>,
    pub macro_path: Option<String>,
    pub morgue_path: Option<String>,
    pub morgue_url: Option<String>,
    pub inprogress_path: Option<String>,
    pub ttyrec_path: Option<String>,
    pub socket_path: Option<String>,
    pub client_path: Option<String>,
    pub dir_path: Option<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub show_save_info: Option<bool>,
    pub allowed_with_hold: Option<bool>,
    pub send_json_options: Option<bool>,
    pub milestone_file: Option<String>,
}

impl ServerConfig {
    /// Load configuration the same way `webtiles.server.run()` does:
    /// defaults, then `<dir>/config.yml` if present, then `<dir>/games.d/*.yml`
    /// unless `games` was already populated by `config.yml`.
    pub fn load(server_path: impl AsRef<Path>) -> Result<Self> {
        let server_path = server_path.as_ref().to_path_buf();
        let mut config = ServerConfig::default();
        config.server_path = server_path.clone();

        let config_yml = server_path.join("config.yml");
        if config_yml.is_file() {
            let text = std::fs::read_to_string(&config_yml)?;
            config.merge_yaml_overrides(&text)?;
        }

        let games_dir = server_path.join("games.d");
        let use_games_dir = config.use_game_yaml.unwrap_or(config.games.is_empty());
        if use_games_dir && games_dir.is_dir() {
            config.load_games_dir(&games_dir)?;
        }

        Ok(config)
    }

    /// Merge a YAML document's top-level keys over the current config.
    /// `games` replaces rather than merges (matching Python); `banned` is
    /// appended.
    pub fn merge_yaml_overrides(&mut self, yaml_text: &str) -> Result<()> {
        let overrides: OverrideDoc = serde_yaml::from_str(yaml_text)
            .map_err(|e| WebtilesError::Config(format!("invalid config.yml: {e}")))?;

        if let Some(games) = overrides.games {
            self.games = games;
        }
        if let Some(templates) = overrides.templates {
            self.templates = templates;
        }
        if let Some(mut banned) = overrides.banned {
            self.banned.append(&mut banned);
        }
        if let Some(v) = overrides.dgl_mode {
            self.dgl_mode = v;
        }
        if let Some(v) = overrides.bind_port {
            self.bind_port = v;
        }
        if let Some(v) = overrides.bind_address {
            self.bind_address = v;
        }
        if let Some(v) = overrides.static_path {
            self.static_path = v;
        }
        if let Some(v) = overrides.template_path {
            self.template_path = v;
        }
        if let Some(v) = overrides.password_db {
            self.password_db = v;
        }
        if let Some(v) = overrides.settings_db {
            self.settings_db = v;
        }
        if let Some(v) = overrides.use_game_yaml {
            self.use_game_yaml = Some(v);
        }
        Ok(())
    }

    fn load_games_dir(&mut self, dir: &Path) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "yml" || e == "yaml").unwrap_or(false))
            .collect();
        entries.sort();

        for path in entries {
            let text = std::fs::read_to_string(&path)?;
            let doc: GamesFileDoc = serde_yaml::from_str(&text).map_err(|e| {
                WebtilesError::Config(format!("invalid games file {}: {e}", path.display()))
            })?;
            for game in doc.games {
                self.games.entry(game.id.clone()).or_insert(game);
            }
            for template in doc.templates {
                // games.d list entries key templates by an inline `id:`
                // field (see games.d/base.yaml), same as games.
                if !template.id.is_empty() {
                    self.templates.entry(template.id.clone()).or_insert(template);
                }
            }
        }
        Ok(())
    }

    /// Apply command-line overrides ([`CliOverrides`]), matching
    /// `export_args_to_config` precedence (CLI wins over everything).
    pub fn apply_cli_overrides(&mut self, args: &CliOverrides) {
        if let Some(port) = args.port {
            self.bind_nonsecure = BindMode::Enabled;
            self.bind_address = String::new();
            self.bind_port = port;
            self.bind_pairs.clear();
            self.ssl_bind_pairs.clear();
        } else if let Some(ssl_port) = args.ssl_port {
            self.bind_nonsecure = BindMode::Disabled;
            self.ssl_bind_pairs = vec![(String::new(), ssl_port)];
        }
        if let Some(daemon) = args.daemon {
            self.daemon = daemon;
        }
        if args.no_pidfile {
            self.pidfile = None;
        }
        if args.live_debug {
            self.live_debug = true;
            self.watch_socket_dirs = false;
            self.daemon = false;
            self.pidfile = None;
        }
    }

    /// Resolve a game by id, applying template inheritance.
    pub fn resolve_game(&self, id: &str) -> Option<ResolvedGame> {
        let game = self.games.get(id)?;
        let template = implicit_template(game.template.as_deref(), id);
        Some(self.resolve(&game.fields, template, id.to_string()))
    }

    fn resolve(&self, fields: &GameFields, template: Option<String>, id: String) -> ResolvedGame {
        let mut seen = std::collections::HashSet::new();
        let mut chain = vec![fields.clone()];
        let mut next = template;
        while let Some(name) = next {
            if !seen.insert(name.clone()) {
                break; // templating loop guard, matches Python's `validate()` loop check
            }
            match self.templates.get(&name) {
                Some(t) => {
                    chain.push(t.fields.clone());
                    next = implicit_template(t.template.as_deref(), &name);
                }
                None => break,
            }
        }
        let mut merged = GameFields::default();
        for f in chain.into_iter().rev() {
            merge_game_fields(&mut merged, f);
        }
        ResolvedGame { id, fields: merged }
    }
}

/// A game/template with no explicit `template:` implicitly inherits from
/// the `default` template, unless it *is* `default`/`base` (Python:
/// `GameConfig.__init__`, `if use_template and template_name is None and
/// not is_metatemplate(self.id): template_name = ... 'default'`).
fn implicit_template(explicit: Option<&str>, self_id: &str) -> Option<String> {
    match explicit {
        Some(name) => Some(name.to_string()),
        None if self_id != "default" && self_id != "base" => Some("default".to_string()),
        None => None,
    }
}

fn merge_game_fields(base: &mut GameFields, overlay: GameFields) {
    macro_rules! over {
        ($field:ident) => {
            if overlay.$field.is_some() {
                base.$field = overlay.$field;
            }
        };
    }
    over!(name);
    over!(version);
    over!(crawl_binary);
    over!(rcfile_path);
    over!(macro_path);
    over!(morgue_path);
    over!(morgue_url);
    over!(inprogress_path);
    over!(ttyrec_path);
    over!(socket_path);
    over!(client_path);
    over!(dir_path);
    over!(cwd);
    over!(show_save_info);
    over!(allowed_with_hold);
    over!(send_json_options);
    over!(milestone_file);
    if !overlay.pre_options.is_empty() {
        base.pre_options = overlay.pre_options;
    }
    if !overlay.options.is_empty() {
        base.options = overlay.options;
    }
    if !overlay.env.is_empty() {
        base.env = overlay.env;
    }
}

/// A game config with templates already resolved - what the rest of the
/// application (process manager, HTTP handlers) actually works with.
#[derive(Debug, Clone)]
pub struct ResolvedGame {
    pub id: String,
    pub fields: GameFields,
}

impl ResolvedGame {
    /// Apply `%n`/`%v`/`%V`/`%r` templating to a single string, matching
    /// `webtiles.config.dgl_format_str`.
    pub fn templated(&self, s: &str, username: Option<&str>) -> Result<String> {
        dgl_format_str(s, username, self.fields.version.as_deref())
    }

    pub fn account_restricted_flag(&self) -> bool {
        // whether -no-player-bones should be passed; decided by caller based
        // on account state, not stored here.
        false
    }
}

/// Port of `webtiles.config.dgl_format_str`. See `PROTOCOL.md` §7.
pub fn dgl_format_str(s: &str, username: Option<&str>, version: Option<&str>) -> Result<String> {
    let mut out = s.to_string();
    if out.contains("%n") {
        match username {
            Some(u) => out = out.replace("%n", u),
            None => {
                return Err(WebtilesError::Config(format!(
                    "username used in config templating but not set: {s}"
                )))
            }
        }
    }
    if out.contains("%v") {
        match version {
            Some(v) => out = out.replace("%v", v),
            None => {
                return Err(WebtilesError::Config(format!(
                    "version used in config templating but not set: {s}"
                )))
            }
        }
    }
    if out.contains("%V") {
        match version {
            Some(v) => out = out.replace("%V", &capitalize(v)),
            None => {
                return Err(WebtilesError::Config(format!(
                    "version used in config templating but not set: {s}"
                )))
            }
        }
    }
    if out.contains("%r") {
        match version {
            Some(v) => {
                let raw = v.splitn(2, "0.").last().unwrap_or(v);
                out = out.replace("%r", raw);
            }
            None => {
                return Err(WebtilesError::Config(format!(
                    "version used in config templating but not set: {s}"
                )))
            }
        }
    }
    Ok(out)
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Command-line overrides, matching `parse_args_main` in `webtiles/server.py`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct CliOverrides {
    /// A port to bind; disables SSL.
    #[arg(short, long)]
    pub port: Option<u16>,
    /// An SSL port to bind. Requires configured SSL cert/key.
    #[arg(long = "ssl-port")]
    pub ssl_port: Option<u16>,
    /// A logfile to write to; use "-" for stdout.
    #[arg(long)]
    pub logfile: Option<String>,
    /// Daemonize after start.
    #[arg(long)]
    pub daemon: Option<bool>,
    /// Do not use a PID-file.
    #[arg(long = "no-pidfile")]
    pub no_pidfile: bool,
    /// Debug mode for server admins (see ARCHITECTURE.md).
    #[arg(long = "live-debug")]
    pub live_debug: bool,
}

#[derive(Debug, Default, Deserialize)]
struct OverrideDoc {
    dgl_mode: Option<bool>,
    bind_port: Option<u16>,
    bind_address: Option<String>,
    static_path: Option<PathBuf>,
    template_path: Option<PathBuf>,
    password_db: Option<PathBuf>,
    settings_db: Option<PathBuf>,
    use_game_yaml: Option<bool>,
    games: Option<BTreeMap<String, GameConfig>>,
    templates: Option<BTreeMap<String, GameTemplate>>,
    banned: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct GamesFileDoc {
    #[serde(default)]
    games: Vec<GameConfig>,
    #[serde(default)]
    templates: Vec<GameTemplate>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python_config_defaults() {
        let c = ServerConfig::default();
        assert!(c.dgl_mode);
        assert_eq!(c.max_connections, 100);
        assert_eq!(c.connection_timeout_secs, 600);
        assert_eq!(c.max_idle_time_secs, 5 * 60 * 60);
        assert_eq!(c.kill_timeout_secs, 10);
        assert_eq!(c.crypt_algorithm, CryptAlgorithm::Broken);
        assert_eq!(c.max_passwd_length, 20);
        assert!(c.allow_anon_spectate);
    }

    #[test]
    fn dgl_format_str_substitutes_all_placeholders() {
        let s = dgl_format_str("%n-%v-%V-%r", Some("alice"), Some("0.34")).unwrap();
        assert_eq!(s, "alice-0.34-0.34-34");
    }

    #[test]
    fn dgl_format_str_errors_when_username_missing() {
        let err = dgl_format_str("%n", None, None).unwrap_err();
        assert!(matches!(err, WebtilesError::Config(_)));
    }

    #[test]
    fn games_yaml_replaces_not_merges() {
        let mut cfg = ServerConfig::default();
        cfg.games.insert(
            "old".to_string(),
            GameConfig {
                id: "old".to_string(),
                ..Default::default()
            },
        );
        cfg.merge_yaml_overrides("games:\n  new:\n    id: new\n")
            .unwrap();
        assert!(!cfg.games.contains_key("old"));
        assert!(cfg.games.contains_key("new"));
    }

    #[test]
    fn banned_list_is_appended_not_replaced() {
        let mut cfg = ServerConfig::default();
        cfg.banned.push("existing".to_string());
        cfg.merge_yaml_overrides("banned:\n  - new_ban\n").unwrap();
        assert_eq!(cfg.banned, vec!["existing".to_string(), "new_ban".to_string()]);
    }

    #[test]
    fn template_inheritance_fills_missing_fields() {
        let mut cfg = ServerConfig::default();
        cfg.templates.insert(
            "default".to_string(),
            GameTemplate {
                fields: GameFields {
                    crawl_binary: Some(PathBuf::from("./crawl")),
                    rcfile_path: Some("./rcs/".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        cfg.games.insert(
            "dcss-web-trunk".to_string(),
            GameConfig {
                id: "dcss-web-trunk".to_string(),
                template: Some("default".to_string()),
                fields: GameFields {
                    name: Some("Play".to_string()),
                    ..Default::default()
                },
            },
        );

        let resolved = cfg.resolve_game("dcss-web-trunk").unwrap();
        assert_eq!(resolved.fields.name.as_deref(), Some("Play"));
        assert_eq!(
            resolved.fields.crawl_binary,
            Some(PathBuf::from("./crawl"))
        );
        assert_eq!(resolved.fields.rcfile_path.as_deref(), Some("./rcs/"));
    }

    #[test]
    fn template_loop_does_not_infinite_loop() {
        let mut cfg = ServerConfig::default();
        // a template that (indirectly) refers back to itself must not
        // infinite-loop resolve_game, since templates can now chain via
        // their own `template` field.
        cfg.templates.insert(
            "a".to_string(),
            GameTemplate { template: Some("a".to_string()), ..Default::default() },
        );
        cfg.games.insert(
            "g".to_string(),
            GameConfig {
                id: "g".to_string(),
                template: Some("a".to_string()),
                fields: GameFields::default(),
            },
        );
        let resolved = cfg.resolve_game("g").unwrap();
        assert_eq!(resolved.id, "g");
    }

    #[test]
    fn real_base_yaml_games_d_file_resolves_correctly() {
        // regression test: games.d list entries key templates by an inline
        // `id:` field, not a `name:` field - a prior bug silently dropped
        // every template (and thus every game, since all of them use one).
        let games_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../webserver/games.d");
        if !games_dir.join("base.yaml").exists() {
            eprintln!("skipping: ../webserver/games.d/base.yaml not found");
            return;
        }
        let mut cfg = ServerConfig::default();
        cfg.load_games_dir(&games_dir).unwrap();

        assert!(!cfg.games.is_empty(), "expected games.d/base.yaml to define games");
        let resolved = cfg
            .resolve_game("dcss-web-trunk")
            .expect("dcss-web-trunk should resolve");
        assert_eq!(resolved.fields.crawl_binary, Some(PathBuf::from("./crawl")));
        assert_eq!(resolved.fields.version.as_deref(), Some("trunk"));
        assert_eq!(resolved.fields.rcfile_path.as_deref(), Some("./rcs/"));
    }
}
