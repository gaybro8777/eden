/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod cache;
#[cfg(fbcode_build)]
mod facebook;

pub use self::cache::{init_cachelib, CachelibSettings};

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::iter::FromIterator;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, format_err, Context, Error, Result};
use cached_config::{ConfigHandle, ConfigStore};
use clap::{App, Arg, ArgGroup, ArgMatches, Values};
use fbinit::FacebookInit;
use maybe_owned::MaybeOwned;
use once_cell::sync::OnceCell;
use panichandler::{self, Fate};
use scribe_ext::Scribe;
use scuba_ext::MononokeScubaSampleBuilder;
use slog::{debug, info, o, warn, Drain, Level, Logger, Never, SendSyncRefUnwindSafeDrain};
use slog_glog_fmt::{kv_categorizer::FacebookCategorizer, kv_defaults::FacebookKV, GlogFormat};
use slog_term::TermDecorator;
use std::panic::{RefUnwindSafe, UnwindSafe};

use blobrepo::BlobRepo;
use blobrepo_factory::{BlobrepoBuilder, Caching, ReadOnlyStorage};
use blobstore_factory::{
    BlobstoreOptions, CachelibBlobstoreOptions, ChaosOptions, PackOptions, PutBehaviour,
    ScrubAction, ThrottleOptions, DEFAULT_PUT_BEHAVIOUR,
};
use metaconfig_parser::{RepoConfigs, StorageConfigs};
use metaconfig_types::{BlobConfig, CommonConfig, Redaction, RepoConfig};
use mononoke_types::RepositoryId;
use observability::{DynamicLevelDrain, ObservabilityContext};
use slog_ext::make_tag_filter_drain;
use sql_construct::SqlConstructFromMetadataDatabaseConfig;
use sql_ext::facebook::{MysqlConnectionType, MysqlOptions, PoolConfig, SharedConnectionPool};
use strum::VariantNames;
use tunables::init_tunables_worker;

use crate::helpers::{create_runtime, setup_repo_dir, CreateStorage};
use crate::log;

pub use self::cache::parse_caching;
use self::cache::{add_cachelib_args, parse_and_init_cachelib};

const CONFIG_PATH: &str = "mononoke-config-path";
const REPO_ID: &str = "repo-id";
const REPO_NAME: &str = "repo-name";
const SOURCE_REPO_GROUP: &str = "source-repo";
const SOURCE_REPO_ID: &str = "source-repo-id";
const SOURCE_REPO_NAME: &str = "source-repo-name";
const TARGET_REPO_GROUP: &str = "target-repo";
const TARGET_REPO_ID: &str = "target-repo-id";
const TARGET_REPO_NAME: &str = "target-repo-name";
const ENABLE_MCROUTER: &str = "enable-mcrouter";
const MYSQL_MYROUTER_PORT: &str = "myrouter-port";
const MYSQL_MASTER_ONLY: &str = "mysql-master-only";
const MYSQL_USE_CLIENT: &str = "use-mysql-client";
const MYSQL_POOL_LIMIT: &str = "mysql-pool-limit";
const MYSQL_POOL_PER_KEY_LIMIT: &str = "mysql-pool-per-key-limit";
const MYSQL_POOL_THREADS_NUM: &str = "mysql-pool-threads-num";
const MYSQL_POOL_AGE_TIMEOUT: &str = "mysql-pool-age-timeout";
const MYSQL_POOL_IDLE_TIMEOUT: &str = "mysql-pool-idle-timeout";
const MYSQL_CONN_OPEN_TIMEOUT: &str = "mysql-conn-open-timeout";
const MYSQL_MAX_QUERY_TIME: &str = "mysql-query-time-limit";
const RUNTIME_THREADS: &str = "runtime-threads";
const TUNABLES_CONFIG: &str = "tunables-config";
const DISABLE_TUNABLES: &str = "disable-tunables";

const DEFAULT_TUNABLES_PATH: &str = "configerator:scm/mononoke/tunables/default";

const READ_QPS_ARG: &str = "blobstore-read-qps";
const WRITE_QPS_ARG: &str = "blobstore-write-qps";
const READ_BYTES_ARG: &str = "blobstore-read-bytes-s";
const WRITE_BYTES_ARG: &str = "blobstore-write-bytes-s";
const READ_BURST_BYTES_ARG: &str = "blobstore-read-burst-bytes-s";
const WRITE_BURST_BYTES_ARG: &str = "blobstore-write-burst-bytes-s";
const BLOBSTORE_BYTES_MIN_THROTTLE_ARG: &str = "blobstore-bytes-min-throttle";
const READ_CHAOS_ARG: &str = "blobstore-read-chaos-rate";
const WRITE_CHAOS_ARG: &str = "blobstore-write-chaos-rate";
const WRITE_ZSTD_ARG: &str = "blobstore-write-zstd-level";
const MANIFOLD_API_KEY_ARG: &str = "manifold-api-key";
const MANIFOLD_USE_CPP_CLIENT_ARG: &str = "manifold-use-cpp-client";
const CACHELIB_ATTEMPT_ZSTD_ARG: &str = "blobstore-cachelib-attempt-zstd";
const BLOBSTORE_PUT_BEHAVIOUR_ARG: &str = "blobstore-put-behaviour";
const BLOBSTORE_SCRUB_ACTION_ARG: &str = "blobstore-scrub-action";
const BLOBSTORE_SCRUB_GRACE_ARG: &str = "blobstore-scrub-grace";

// Old version took no args which means it would be no good for overriding default for a binary that defaults to true.
const READONLY_STORAGE_OLD_ARG: &str = "readonly-storage";
const READONLY_STORAGE_NEW_ARG: &str = "with-readonly-storage";

const LOG_INCLUDE_TAG: &str = "log-include-tag";
const LOG_EXCLUDE_TAG: &str = "log-exclude-tag";
// Argument, responsible for instantiation of `ObservabilityContext::Dynamic`
const WITH_DYNAMIC_OBSERVABILITY: &str = "with-dynamic-observability";

const LOCAL_CONFIGERATOR_PATH_ARG: &str = "local-configerator-path";
const CRYPTO_PATH_REGEX_ARG: &str = "crypto-path-regex";
const CRYPTO_PROJECT: &str = "SCM";

const CONFIGERATOR_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIGERATOR_REFRESH_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgType {
    /// Options related to mononoke config
    Config,
    /// Options related to stderr logging,
    Logging,
    /// Adds options related to mysql database connections
    Mysql,
    /// Adds options related to blobstore access
    Blobstore,
    /// Adds options related to cachelib and its use from blobstore
    Cachelib,
    /// Adds options related to tokio runtime
    Runtime,
    /// Adds options related to mononoke tunables
    Tunables,
    /// Adds options to select a repo. If not present then all repos.
    Repo,
    /// Adds options for scrubbing blobstores
    Scrub,
    /// Adds --source-repo-id/repo-name and --target-repo-id/repo-name options.
    /// Necessary for crossrepo operations
    /// Only visible if Repo group is visible.
    SourceAndTargetRepos,
    /// Adds just --source-repo-id/repo-name, for blobimport into a megarepo
    /// Only visible if Repo group is visible.
    SourceRepo,
    /// Adds --shutdown-grace-period and --shutdown-timeout for graceful shutdown.
    ShutdownTimeouts,
    /// Adds --scuba-dataset and --scuba-log-file for scuba logging.
    ScubaLogging,
    /// Adds --disabled-hooks for disabling hooks.
    DisableHooks,
    /// Adds --fb303-thrift-port for stats and profiling
    Fb303,
}

// Arguments that are enabled by default for MononokeAppBuilder
const DEFAULT_ARG_TYPES: &[ArgType] = &[
    ArgType::Blobstore,
    ArgType::Cachelib,
    ArgType::Config,
    ArgType::Logging,
    ArgType::Mysql,
    ArgType::Repo,
    ArgType::Runtime,
    ArgType::Tunables,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoRequirement {
    ExactlyOne,
    AtLeastOne,
}

/// Build clap App with appropriate default settings.
pub struct MononokeAppBuilder {
    /// The app name.
    name: String,

    /// Whether to hide advanced Manifold configuration from help. Note that the arguments will
    /// still be available, just not displayed in help.
    hide_advanced_args: bool,

    /// Whether to require the user select a repo if the option is present.
    repo_required: Option<RepoRequirement>,

    /// Which groups of arguments are enabled for this app
    arg_types: HashSet<ArgType>,

    /// This app is special admin tool, needs to run with specific PutBehaviour
    special_put_behaviour: Option<PutBehaviour>,

    /// Cachelib default settings, as shown in usage
    cachelib_settings: CachelibSettings,

    /// Whether to default to readonly storage or not
    readonly_storage_default: ReadOnlyStorage,

    /// Whether to default to attempting to compress to cachelib for large objects
    blobstore_cachelib_attempt_zstd_default: bool,

    /// Whether to default to limit blobstore read QPS
    blobstore_read_qps_default: Option<NonZeroU32>,

    /// The default Scuba dataset for this app, if any.
    default_scuba_dataset: Option<String>,

    // Whether to default to scrubbing when using a multiplexed blobstore
    scrub_action_default: Option<ScrubAction>,

    // Whether to allow a grace period before reporting a key missing in a store for recent keys
    scrub_grace_secs_default: Option<u64>,
}

/// Things we want to live for the lifetime of the mononoke binary
#[derive(Default)]
pub struct MononokeAppData {
    cachelib_settings: CachelibSettings,
    repo_required: Option<RepoRequirement>,
    global_mysql_connection_pool: SharedConnectionPool,
    default_scuba_dataset: Option<String>,
}

// Result of MononokeAppBuilder::build() which has clap plus the MononokeApp data
pub struct MononokeClapApp<'a, 'b> {
    clap: App<'a, 'b>,
    app_data: MononokeAppData,
    arg_types: HashSet<ArgType>,
}

impl<'a, 'b> MononokeClapApp<'a, 'b> {
    pub fn about<S: Into<&'b str>>(self, about: S) -> Self {
        Self {
            clap: self.clap.about(about),
            ..self
        }
    }

    pub fn subcommand(self, subcmd: App<'a, 'b>) -> Self {
        Self {
            clap: self.clap.subcommand(subcmd),
            ..self
        }
    }

    pub fn arg<A: Into<Arg<'a, 'b>>>(mut self, a: A) -> Self {
        self.clap.p.add_arg(a.into());
        self
    }

    pub fn args_from_usage(self, usage: &'a str) -> Self {
        Self {
            clap: self.clap.args_from_usage(usage),
            ..self
        }
    }

    pub fn group(self, group: ArgGroup<'a>) -> Self {
        Self {
            clap: self.clap.group(group),
            ..self
        }
    }

    pub fn get_matches(self) -> MononokeMatches<'a> {
        MononokeMatches {
            matches: MaybeOwned::from(self.clap.get_matches()),
            app_data: self.app_data,
            arg_types: self.arg_types,
        }
    }

    pub fn get_matches_from<I, T>(self, itr: I) -> MononokeMatches<'a>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        MononokeMatches {
            matches: MaybeOwned::from(self.clap.get_matches_from(itr)),
            app_data: self.app_data,
            arg_types: self.arg_types,
        }
    }
}

#[derive(Default)]
pub struct MononokeMatches<'a> {
    matches: MaybeOwned<'a, ArgMatches<'a>>,
    app_data: MononokeAppData,
    arg_types: HashSet<ArgType>,
}

impl<'a> MononokeMatches<'a> {
    pub fn parse_and_init_cachelib(&self, fb: FacebookInit) -> Caching {
        parse_and_init_cachelib(fb, &self.matches, self.app_data.cachelib_settings.clone())
    }

    pub fn init_mononoke(
        &'a self,
        fb: FacebookInit,
    ) -> Result<(Caching, Logger, tokio::runtime::Runtime)> {
        init_mononoke_with_cache_settings(fb, self, self.app_data.cachelib_settings.clone())
    }

    // Delegate some common methods to save on .as_ref() calls
    pub fn is_present<S: AsRef<str>>(&self, name: S) -> bool {
        self.matches.is_present(name)
    }

    pub fn subcommand(&'a self) -> (&str, Option<&'a ArgMatches<'a>>) {
        self.matches.subcommand()
    }

    pub fn usage(&self) -> &str {
        self.matches.usage()
    }

    pub fn value_of<S: AsRef<str>>(&self, name: S) -> Option<&str> {
        self.matches.value_of(name)
    }

    pub fn value_of_os<S: AsRef<str>>(&self, name: S) -> Option<&OsStr> {
        self.matches.value_of_os(name)
    }

    pub fn values_of<S: AsRef<str>>(&'a self, name: S) -> Option<Values<'a>> {
        self.matches.values_of(name)
    }
}

impl<'a> AsRef<ArgMatches<'a>> for MononokeMatches<'a> {
    fn as_ref(&self) -> &ArgMatches<'a> {
        &self.matches
    }
}

impl<'a> Borrow<ArgMatches<'a>> for MononokeMatches<'a> {
    fn borrow(&self) -> &ArgMatches<'a> {
        &self.matches
    }
}

/// Create a default root logger for Facebook services
fn glog_drain() -> impl Drain<Ok = (), Err = Never> {
    let decorator = TermDecorator::new().build();
    // FacebookCategorizer is used for slog KV arguments.
    // At the time of writing this code FacebookCategorizer and FacebookKV
    // that was added below was mainly useful for logview logging and had no effect on GlogFormat
    let drain = GlogFormat::new(decorator, FacebookCategorizer).ignore_res();
    ::std::sync::Mutex::new(drain).ignore_res()
}

/// Create a `Drain` whose `Level` is dynamically read from the `ConfigStore`
fn dynamic_level_drain<'a>(
    fb: FacebookInit,
    matches: &'a MononokeMatches<'a>,
    inner_drain: impl Drain<Ok = (), Err = Never>
    + Clone
    + Send
    + Sync
    + UnwindSafe
    + RefUnwindSafe
    + 'static,
) -> Result<impl Drain<Ok = (), Err = Never>, Error> {
    let kv = FacebookKV::new().expect("cannot initialize FacebookKV");
    let logger = Logger::root(inner_drain.clone(), o![kv]);
    let observability_context = init_observability_context(fb, matches, Some(&logger))?;
    Ok(DynamicLevelDrain::new(inner_drain, observability_context))
}

impl MononokeAppBuilder {
    /// Start building a new Mononoke app.  This adds the standard Mononoke args.  Use the `build`
    /// method to get a `clap::App` that you can then customize further.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            hide_advanced_args: false,
            repo_required: None,
            arg_types: HashSet::from_iter(DEFAULT_ARG_TYPES.iter().cloned()),
            special_put_behaviour: None,
            cachelib_settings: CachelibSettings::default(),
            readonly_storage_default: ReadOnlyStorage(false),
            blobstore_cachelib_attempt_zstd_default: true,
            blobstore_read_qps_default: None,
            default_scuba_dataset: None,
            scrub_action_default: None,
            scrub_grace_secs_default: None,
        }
    }

    /// Hide advanced args.
    pub fn with_advanced_args_hidden(mut self) -> Self {
        self.hide_advanced_args = true;
        self
    }

    /// This command operates on all configured repos, and removes the options for selecting a
    /// repo.  The default behaviour is for the arguments to specify the repo to be optional, which is
    /// probably not what you want, so you should call either this method or `with_repo_required`.
    pub fn with_all_repos(mut self) -> Self {
        self.arg_types.remove(&ArgType::Repo);
        self
    }

    /// This command operates on a specific repos, so this makes the options for selecting a
    /// repo required.  The default behaviour is for the arguments to specify the repo to be
    /// optional, which is probably not what you want, so you should call either this method or
    /// `with_all_repos`.
    pub fn with_repo_required(mut self, requirement: RepoRequirement) -> Self {
        self.arg_types.insert(ArgType::Repo);
        self.repo_required = Some(requirement);
        self
    }

    /// This command might operate on two repos in the same time. This is normally used
    /// for two repos where one repo is synced into another.
    pub fn with_source_and_target_repos(mut self) -> Self {
        self.arg_types.insert(ArgType::SourceAndTargetRepos);
        self
    }

    /// This command operates on one repo (--repo-id/name), but needs to be aware that commits
    /// are sourced from another repo.
    pub fn with_source_repos(mut self) -> Self {
        self.arg_types.insert(ArgType::SourceRepo);
        self
    }

    /// This command has arguments for graceful shutdown.
    pub fn with_shutdown_timeout_args(mut self) -> Self {
        self.arg_types.insert(ArgType::ShutdownTimeouts);
        self
    }

    /// This command has arguments for scuba logging.
    pub fn with_scuba_logging_args(mut self) -> Self {
        self.arg_types.insert(ArgType::ScubaLogging);
        self
    }

    /// This command has arguments for disabled hooks.
    pub fn with_disabled_hooks_args(mut self) -> Self {
        self.arg_types.insert(ArgType::DisableHooks);
        self
    }

    /// This command has arguments for fb303
    pub fn with_fb303_args(mut self) -> Self {
        self.arg_types.insert(ArgType::Fb303);
        self
    }

    pub fn with_default_scuba_dataset(mut self, default: impl Into<String>) -> Self {
        self.default_scuba_dataset = Some(default.into());
        self
    }

    /// This command does expose these types of arguments
    pub fn with_arg_types<I>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = ArgType>,
    {
        for t in types {
            self.arg_types.insert(t);
        }
        self
    }

    /// This command does not expose these types of arguments
    pub fn without_arg_types<I>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = ArgType>,
    {
        for t in types {
            self.arg_types.remove(&t);
        }
        self
    }

    /// This command needs a special default put behaviour (e.g. its an admin tool)
    pub fn with_special_put_behaviour(mut self, put_behaviour: PutBehaviour) -> Self {
        self.special_put_behaviour = Some(put_behaviour);
        self
    }

    /// This command has a special default readonly storage setting
    pub fn with_readonly_storage_default(mut self, v: ReadOnlyStorage) -> Self {
        self.readonly_storage_default = v;
        self
    }

    /// This command has a special default blobstore_cachelib_attempt_zstd setting
    pub fn with_blobstore_cachelib_attempt_zstd_default(mut self, d: bool) -> Self {
        self.blobstore_cachelib_attempt_zstd_default = d;
        self
    }

    /// This command has a special default blobstore_read_qps default setting
    pub fn with_blobstore_read_qps_default(mut self, d: Option<NonZeroU32>) -> Self {
        self.blobstore_read_qps_default = d;
        self
    }

    /// This command has different cachelib defaults, show them in --help
    pub fn with_cachelib_settings(mut self, cachelib_settings: CachelibSettings) -> Self {
        self.cachelib_settings = cachelib_settings;
        self
    }

    /// This command has a special scrub_action default setting
    pub fn with_scrub_action_default(mut self, d: Option<ScrubAction>) -> Self {
        self.scrub_action_default = d;
        self
    }

    /// This command has a special grace period for recent keys when scrubbing
    pub fn with_scrub_grace_secs_default(mut self, d: Option<u64>) -> Self {
        self.scrub_grace_secs_default = d;
        self
    }

    /// Build a MononokeClapApp around a `clap::App` for this Mononoke app, which can then be customized further.
    pub fn build<'a, 'b>(self) -> MononokeClapApp<'a, 'b> {
        let mut app = App::new(self.name.clone());

        if self.arg_types.contains(&ArgType::Config) {
            app = app.arg(
                Arg::with_name(CONFIG_PATH)
                    .long(CONFIG_PATH)
                    .value_name("MONONOKE_CONFIG_PATH")
                    .help("Path to the Mononoke configs"),
            )
            .arg(
                Arg::with_name(CRYPTO_PATH_REGEX_ARG)
                    .multiple(true)
                    .long(CRYPTO_PATH_REGEX_ARG)
                    .takes_value(true)
                    .help("Regex for a Configerator path that must be covered by Mononoke's crypto project")
            )
            .arg(
                Arg::with_name(LOCAL_CONFIGERATOR_PATH_ARG)
                    .long(LOCAL_CONFIGERATOR_PATH_ARG)
                    .takes_value(true)
                    .help("local path to fetch configerator configs from, instead of normal configerator"),
            );
        }

        if self.arg_types.contains(&ArgType::Repo) {
            let repo_conflicts: &[&str] = if self.arg_types.contains(&ArgType::SourceRepo) {
                &[TARGET_REPO_ID, TARGET_REPO_NAME]
            } else {
                &[
                    SOURCE_REPO_ID,
                    SOURCE_REPO_NAME,
                    TARGET_REPO_ID,
                    TARGET_REPO_NAME,
                ]
            };

            let mut repo_id_arg = Arg::with_name(REPO_ID)
                .long(REPO_ID)
                // This is an old form that some consumers use
                .alias("repo_id")
                .value_name("ID")
                .help("numeric ID of repository")
                .conflicts_with_all(repo_conflicts);

            let mut repo_name_arg = Arg::with_name(REPO_NAME)
                .long(REPO_NAME)
                .value_name("NAME")
                .help("Name of repository")
                .conflicts_with_all(repo_conflicts);

            let mut repo_group = ArgGroup::with_name("repo")
                .args(&[REPO_ID, REPO_NAME])
                .required(self.repo_required.is_some());

            if self.repo_required == Some(RepoRequirement::AtLeastOne) {
                repo_id_arg = repo_id_arg.multiple(true).number_of_values(1);
                repo_name_arg = repo_name_arg.multiple(true).number_of_values(1);
                repo_group = repo_group.multiple(true)
            }

            app = app.arg(repo_id_arg).arg(repo_name_arg).group(repo_group);

            if self.arg_types.contains(&ArgType::SourceRepo)
                || self.arg_types.contains(&ArgType::SourceAndTargetRepos)
            {
                app = app
                    .arg(
                        Arg::with_name(SOURCE_REPO_ID)
                        .long(SOURCE_REPO_ID)
                        .value_name("ID")
                        .help("numeric ID of source repository (used only for commands that operate on more than one repo)"),
                    )
                    .arg(
                        Arg::with_name(SOURCE_REPO_NAME)
                        .long(SOURCE_REPO_NAME)
                        .value_name("NAME")
                        .help("Name of source repository (used only for commands that operate on more than one repo)"),
                    )
                    .group(
                        ArgGroup::with_name(SOURCE_REPO_GROUP)
                            .args(&[SOURCE_REPO_ID, SOURCE_REPO_NAME])
                    )
            }

            if self.arg_types.contains(&ArgType::SourceAndTargetRepos) {
                app = app
                    .arg(
                        Arg::with_name(TARGET_REPO_ID)
                        .long(TARGET_REPO_ID)
                        .value_name("ID")
                        .help("numeric ID of target repository (used only for commands that operate on more than one repo)"),
                    )
                    .arg(
                        Arg::with_name(TARGET_REPO_NAME)
                        .long(TARGET_REPO_NAME)
                        .value_name("NAME")
                        .help("Name of target repository (used only for commands that operate on more than one repo)"),
                    )
                    .group(
                        ArgGroup::with_name(TARGET_REPO_GROUP)
                            .args(&[TARGET_REPO_ID, TARGET_REPO_NAME])
                    );
            }
        }

        if self.arg_types.contains(&ArgType::Logging) {
            app = add_logger_args(app);
        }
        if self.arg_types.contains(&ArgType::Mysql) {
            app = add_mysql_options_args(app);
        }
        if self.arg_types.contains(&ArgType::Blobstore) {
            app = self.add_blobstore_args(app);
        }
        if self.arg_types.contains(&ArgType::Cachelib) {
            app = add_cachelib_args(app, self.hide_advanced_args, self.cachelib_settings.clone());
        }
        if self.arg_types.contains(&ArgType::Runtime) {
            app = add_runtime_args(app);
        }
        if self.arg_types.contains(&ArgType::Tunables) {
            app = add_tunables_args(app);
        }
        if self.arg_types.contains(&ArgType::ShutdownTimeouts) {
            app = add_shutdown_timeout_args(app);
        }
        if self.arg_types.contains(&ArgType::ScubaLogging) {
            app = add_scuba_logging_args(app, self.default_scuba_dataset.is_some());
        }
        if self.arg_types.contains(&ArgType::DisableHooks) {
            app = add_disabled_hooks_args(app);
        }
        if self.arg_types.contains(&ArgType::Fb303) {
            app = add_fb303_args(app);
        }

        MononokeClapApp {
            clap: app,
            app_data: MononokeAppData {
                cachelib_settings: self.cachelib_settings,
                repo_required: self.repo_required,
                global_mysql_connection_pool: SharedConnectionPool::new(),
                default_scuba_dataset: self.default_scuba_dataset,
            },
            arg_types: self.arg_types,
        }
    }

    fn add_blobstore_args<'a, 'b>(&self, app: App<'a, 'b>) -> App<'a, 'b> {
        let mut put_arg = Arg::with_name(BLOBSTORE_PUT_BEHAVIOUR_ARG)
            .long(BLOBSTORE_PUT_BEHAVIOUR_ARG)
            .takes_value(true)
            .required(false)
            .help("Desired blobstore behaviour when a put is made to an existing key.");

        if let Some(special_put_behaviour) = self.special_put_behaviour {
            put_arg = put_arg.default_value(special_put_behaviour.into());
        } else {
            // Add the default here so that it shows in --help
            put_arg = put_arg.default_value(DEFAULT_PUT_BEHAVIOUR.into());
        }

        let mut read_qps_arg = Arg::with_name(READ_QPS_ARG)
            .long(READ_QPS_ARG)
            .takes_value(true)
            .required(false)
            .help("Read QPS limit to ThrottledBlob");

        if let Some(default) = self.blobstore_read_qps_default {
            // Lazy static is nicer to LeakSanitizer than Box::leak
            static QPS_FORMATTED: OnceCell<String> = OnceCell::new();
            // clap needs &'static str
            read_qps_arg =
                read_qps_arg.default_value(&QPS_FORMATTED.get_or_init(|| format!("{}", default)));
        }

        let app = app.arg(
           read_qps_arg
        )
        .arg(
            Arg::with_name(WRITE_QPS_ARG)
                .long(WRITE_QPS_ARG)
                .takes_value(true)
                .required(false)
                .help("Write QPS limit to ThrottledBlob"),
        )
        .arg(
            Arg::with_name(WRITE_BYTES_ARG)
                .long(WRITE_BYTES_ARG)
                .takes_value(true)
                .required(false)
                .help("Write Bytes/s limit to ThrottledBlob"),
        )
        .arg(
            Arg::with_name(READ_BYTES_ARG)
                .long(READ_BYTES_ARG)
                .takes_value(true)
                .required(false)
                .help("Read Bytes/s limit to ThrottledBlob"),
        )
        .arg(
            Arg::with_name(READ_BURST_BYTES_ARG)
                .long(READ_BURST_BYTES_ARG)
                .takes_value(true)
                .required(false)
                .help("Maximum burst bytes/s limit to ThrottledBlob.  Blobs larger than this will error rather than throttle due to consuming too much quota."),
        )
        .arg(
            Arg::with_name(WRITE_BURST_BYTES_ARG)
                .long(WRITE_BURST_BYTES_ARG)
                .takes_value(true)
                .required(false)
                .help("Maximum burst bytes/s limit to ThrottledBlob.  Blobs larger than this will error rather than throttle due to consuming too much quota."),
        )
        .arg(
            Arg::with_name(BLOBSTORE_BYTES_MIN_THROTTLE_ARG)
                .long(BLOBSTORE_BYTES_MIN_THROTTLE_ARG)
                .takes_value(true)
                .required(false)
                .help("Minimum number of bytes ThrottledBlob can count"),
        )
        .arg(
            Arg::with_name(READ_CHAOS_ARG)
                .long(READ_CHAOS_ARG)
                .takes_value(true)
                .required(false)
                .help("Rate of errors on reads. Pass N,  it will error randomly 1/N times. For multiplexed stores will only apply to the first store in the multiplex."),
        )
        .arg(
            Arg::with_name(WRITE_CHAOS_ARG)
                .long(WRITE_CHAOS_ARG)
                .takes_value(true)
                .required(false)
                .help("Rate of errors on writes. Pass N,  it will error randomly 1/N times. For multiplexed stores will only apply to the first store in the multiplex."),
        )
        .arg(
            Arg::with_name(WRITE_ZSTD_ARG)
                .long(WRITE_ZSTD_ARG)
                .takes_value(true)
                .required(false)
                .help("Set the zstd compression level to be used on writes via the packed blobstore (if configured).  Default is None."),
        )
        .arg(
            Arg::with_name(MANIFOLD_API_KEY_ARG)
                .long(MANIFOLD_API_KEY_ARG)
                .takes_value(true)
                .required(false)
                .help("Manifold API key"),
        )
        .arg(
            Arg::with_name(MANIFOLD_USE_CPP_CLIENT_ARG)
                .long(MANIFOLD_USE_CPP_CLIENT_ARG)
                .takes_value(true)
                .possible_values(BOOL_VALUES)
                .required(false)
                .default_value(bool_as_str(false))
                .help("Whether to allow Manifold blobstore to use the C++ client"),
        )
        .arg(
            Arg::with_name(CACHELIB_ATTEMPT_ZSTD_ARG)
                .long(CACHELIB_ATTEMPT_ZSTD_ARG)
                .takes_value(true)
                .possible_values(BOOL_VALUES)
                .required(false)
                .default_value(bool_as_str(self.blobstore_cachelib_attempt_zstd_default))
                .help("Whether to attempt zstd compression when blobstore is putting things into cachelib over threshold size."),
        )
        .arg(
          put_arg
        )
        .arg(
            Arg::with_name(READONLY_STORAGE_OLD_ARG)
                .long(READONLY_STORAGE_OLD_ARG)
                .help("Error on any attempts to write to storage. DEPRECATED, prefer --with-readonly-storage=<true|false>"),
        )
        .arg(
            Arg::with_name(READONLY_STORAGE_NEW_ARG)
                .long(READONLY_STORAGE_NEW_ARG)
                .takes_value(true)
                .possible_values(BOOL_VALUES)
                .default_value(bool_as_str(self.readonly_storage_default.0))
                .help("Error on any attempts to write to storage if set to true"),
        );

        if self.arg_types.contains(&ArgType::Scrub) {
            let mut scrub_action_arg = Arg::with_name(BLOBSTORE_SCRUB_ACTION_ARG)
                .long(BLOBSTORE_SCRUB_ACTION_ARG)
                .takes_value(true)
                .required(false)
                .possible_values(ScrubAction::VARIANTS)
                .help("Enable ScrubBlobstore with the given action. Checks for keys missing from stores. In ReportOnly mode this logs only, otherwise it performs a copy to the missing stores.");
            if let Some(default) = self.scrub_action_default {
                scrub_action_arg = scrub_action_arg.default_value(default.into());
            }
            let mut scrub_grace_arg = Arg::with_name(BLOBSTORE_SCRUB_GRACE_ARG)
                .long(BLOBSTORE_SCRUB_GRACE_ARG)
                .takes_value(true)
                .required(false)
                .help("Number of seconds grace to give for key to arrive in multiple blobstores or the healer queue when scrubbing");
            if let Some(default) = self.scrub_grace_secs_default {
                static FORMATTED: OnceCell<String> = OnceCell::new(); // Lazy static is nicer to LeakSanitizer than Box::leak
                scrub_grace_arg = scrub_grace_arg
                    .default_value(&FORMATTED.get_or_init(|| format!("{}", default)));
            }
            app.arg(scrub_action_arg).arg(scrub_grace_arg)
        } else {
            app
        }
    }
}

fn add_tunables_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name(TUNABLES_CONFIG)
            .long(TUNABLES_CONFIG)
            .takes_value(true)
            .help("The location of a tunables config"),
    )
    .arg(
        Arg::with_name(DISABLE_TUNABLES)
            .long(DISABLE_TUNABLES)
            .help("Use the default values for all tunables (useful for tests)"),
    )
}
fn add_runtime_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name(RUNTIME_THREADS)
            .long(RUNTIME_THREADS)
            .takes_value(true)
            .help("a number of threads to use in the tokio runtime"),
    )
}

fn add_logger_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("panic-fate")
            .long("panic-fate")
            .value_name("PANIC_FATE")
            .possible_values(&["continue", "exit", "abort"])
            .default_value("abort")
            .help("fate of the process when a panic happens"),
    )
    .arg(
        Arg::with_name("logview-category")
            .long("logview-category")
            .takes_value(true)
            .help("logview category to log to. Logview is not used if not set"),
    )
    .arg(
        Arg::with_name("debug")
            .short("d")
            .long("debug")
            .help("print debug output"),
    )
    .arg(
        Arg::with_name("log-level")
            .long("log-level")
            .help("log level to use (does not work with --debug)")
            .takes_value(true)
            .possible_values(&["CRITICAL", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"])
            .conflicts_with("debug"),
    )
    .arg(
        Arg::with_name(LOG_INCLUDE_TAG)
            .long(LOG_INCLUDE_TAG)
            .short("l")
            .help("include only log messages with these slog::Record::tags()/log::Record::targets")
            .takes_value(true)
            .multiple(true)
            .number_of_values(1),
    )
    .arg(
        Arg::with_name(LOG_EXCLUDE_TAG)
            .long(LOG_EXCLUDE_TAG)
            .short("L")
            .help("exclude log messages with these slog::Record::tags()/log::Record::targets")
            .takes_value(true)
            .multiple(true)
            .number_of_values(1),
    )
    .arg(
        Arg::with_name(WITH_DYNAMIC_OBSERVABILITY)
            .long(WITH_DYNAMIC_OBSERVABILITY)
            .help(
                "whether to instantiate ObservabilityContext::Dynamic,\
                 which reads logging levels from configerator. Overwrites\
                 --log-level or --debug",
            )
            .takes_value(true)
            .possible_values(&["true", "false"])
            .default_value("false"),
    )
}

fn get_log_level<'a>(matches: &MononokeMatches<'a>) -> Level {
    if matches.is_present("debug") {
        Level::Debug
    } else {
        match matches.value_of("log-level") {
            Some(log_level_str) => Level::from_str(log_level_str)
                .unwrap_or_else(|_| panic!("Unknown log level: {}", log_level_str)),
            None => Level::Info,
        }
    }
}

pub fn init_logging<'a>(fb: FacebookInit, matches: &MononokeMatches<'a>) -> Result<Logger> {
    // Set the panic handler up here. Not really relevent to logger other than it emits output
    // when things go wrong. This writes directly to stderr as coredumper expects.
    let fate = match matches
        .value_of("panic-fate")
        .expect("no default on panic-fate")
    {
        "none" => None,
        "continue" => Some(Fate::Continue),
        "exit" => Some(Fate::Exit(101)),
        "abort" => Some(Fate::Abort),
        bad => bail!("bad panic-fate {}", bad),
    };
    if let Some(fate) = fate {
        panichandler::set_panichandler(fate);
    }

    let stdlog_env = "RUST_LOG";

    let glog_drain = make_tag_filter_drain(
        glog_drain(),
        matches
            .values_of(LOG_INCLUDE_TAG)
            .map(|v| v.map(|v| v.to_string()).collect())
            .unwrap_or_default(),
        matches
            .values_of(LOG_EXCLUDE_TAG)
            .map(|v| v.map(|v| v.to_string()).collect())
            .unwrap_or_default(),
        true, // Log messages which have no tags
    )?;

    let root_log_drain: Arc<dyn SendSyncRefUnwindSafeDrain<Ok = (), Err = Never>> = match matches
        .value_of("logview-category")
    {
        Some(category) => {
            #[cfg(fbcode_build)]
            {
                // Sometimes scribe writes can fail due to backpressure - it's OK to drop these
                // since logview is sampled anyway.
                let logview_drain = ::slog_logview::LogViewDrain::new(fb, category).ignore_res();
                let drain = slog::Duplicate::new(glog_drain, logview_drain);
                Arc::new(drain.ignore_res())
            }
            #[cfg(not(fbcode_build))]
            {
                let _ = (fb, category);
                unimplemented!(
                    "Passed --logview-category, but it is supported only for fbcode builds"
                )
            }
        }
        None => Arc::new(glog_drain),
    };

    // NOTE: We pass an unfiltered Logger to init_stdlog_once. That's because we do the filtering
    // at the stdlog level there.
    let stdlog_level =
        log::init_stdlog_once(Logger::root(root_log_drain.clone(), o![]), stdlog_env);

    let root_log_drain = dynamic_level_drain(fb, matches, root_log_drain)
        .with_context(|| "Failed to initialize DynamicLevelDrain")?;

    let kv = FacebookKV::new().expect("cannot initialize FacebookKV");
    let logger = if matches.is_present("fb303-thrift-port") {
        Logger::root(slog_stats::StatsDrain::new(root_log_drain), o![kv])
    } else {
        Logger::root(root_log_drain, o![kv])
    };

    debug!(
        logger,
        "enabled stdlog with level: {:?} (set {} to configure)", stdlog_level, stdlog_env
    );

    Ok(logger)
}

fn get_repo_id_and_name_from_values<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
    option_repo_name: &str,
    option_repo_id: &str,
) -> Result<(RepositoryId, String)> {
    let resolved = resolve_repo(config_store, matches, option_repo_name, option_repo_id)?;
    Ok((resolved.id, resolved.name))
}

pub struct ResolvedRepo {
    pub id: RepositoryId,
    pub name: String,
    pub config: RepoConfig,
}

pub fn resolve_repo<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
    option_repo_name: &str,
    option_repo_id: &str,
) -> Result<ResolvedRepo> {
    let repo_name = matches.value_of(option_repo_name);
    let repo_id = matches.value_of(option_repo_id);
    let configs = load_repo_configs(config_store, matches)?;
    match (repo_name, repo_id) {
        (Some(_), Some(_)) => bail!("both repo-name and repo-id parameters set"),
        (None, None) => bail!("neither repo-name nor repo-id parameter set"),
        (None, Some(repo_id)) => resolve_repo_given_id(RepositoryId::from_str(repo_id)?, &configs),
        (Some(repo_name), None) => resolve_repo_given_name(repo_name, &configs),
    }
}

pub fn resolve_repos<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<Vec<ResolvedRepo>> {
    resolve_repos_from_args(config_store, matches, REPO_NAME, REPO_ID)
}

fn resolve_repos_from_args<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
    option_repo_name: &str,
    option_repo_id: &str,
) -> Result<Vec<ResolvedRepo>> {
    if matches.app_data.repo_required == Some(RepoRequirement::ExactlyOne) {
        return resolve_repo(config_store, matches, option_repo_name, option_repo_id)
            .map(|r| vec![r]);
    }

    let repo_names = matches.values_of(option_repo_name);
    let repo_ids = matches.values_of(option_repo_id);
    let configs = load_repo_configs(config_store, matches)?;

    let mut repos = Vec::new();
    let mut names = HashSet::new();
    if let Some(repo_ids) = repo_ids {
        for i in repo_ids {
            let resolved = resolve_repo_given_id(RepositoryId::from_str(i)?, &configs)?;
            if names.insert(resolved.name.clone()) {
                repos.push(resolved);
            }
        }
    }
    if let Some(repo_names) = repo_names {
        for n in repo_names {
            let resolved = resolve_repo_given_name(n, &configs)?;
            if names.insert(n.to_string()) {
                repos.push(resolved)
            }
        }
    }
    if repos.is_empty() {
        bail!("neither repo-name nor repo-id parameters set");
    }
    Ok(repos)
}

fn resolve_repo_given_id(id: RepositoryId, configs: &RepoConfigs) -> Result<ResolvedRepo> {
    let config = configs
        .repos
        .iter()
        .filter(|(_, c)| c.repoid == id)
        .enumerate()
        .last();
    if let Some((count, (name, config))) = config {
        if count > 1 {
            Err(format_err!("multiple configs defined for repo-id {:?}", id))
        } else {
            Ok(ResolvedRepo {
                id,
                name: name.to_string(),
                config: config.clone(),
            })
        }
    } else {
        Err(format_err!("unknown config for repo-id {:?}", id))
    }
}

fn resolve_repo_given_name(name: &str, configs: &RepoConfigs) -> Result<ResolvedRepo> {
    let config = configs.repos.get(name);
    if let Some(config) = config {
        Ok(ResolvedRepo {
            id: config.repoid,
            name: name.to_string(),
            config: config.clone(),
        })
    } else {
        Err(format_err!("unknown repo-name {:?}", name))
    }
}

pub fn get_repo_id<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<RepositoryId> {
    let (repo_id, _) = get_repo_id_and_name_from_values(config_store, matches, REPO_NAME, REPO_ID)?;
    Ok(repo_id)
}

pub fn get_repo_name<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<String> {
    let (_, repo_name) =
        get_repo_id_and_name_from_values(config_store, matches, REPO_NAME, REPO_ID)?;
    Ok(repo_name)
}

pub fn get_source_repo_id<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<RepositoryId> {
    let (repo_id, _) =
        get_repo_id_and_name_from_values(config_store, matches, SOURCE_REPO_NAME, SOURCE_REPO_ID)?;
    Ok(repo_id)
}

pub fn get_source_repo_id_opt<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<Option<RepositoryId>> {
    if matches.is_present(SOURCE_REPO_NAME) || matches.is_present(SOURCE_REPO_ID) {
        let (repo_id, _) = get_repo_id_and_name_from_values(
            config_store,
            matches,
            SOURCE_REPO_NAME,
            SOURCE_REPO_ID,
        )?;
        Ok(Some(repo_id))
    } else {
        Ok(None)
    }
}

pub fn get_target_repo_id<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<RepositoryId> {
    let (repo_id, _) =
        get_repo_id_and_name_from_values(config_store, matches, TARGET_REPO_NAME, TARGET_REPO_ID)?;
    Ok(repo_id)
}

pub fn get_repo_id_from_value<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
    repo_id_arg: &str,
) -> Result<RepositoryId> {
    let (repo_id, _) = get_repo_id_and_name_from_values(config_store, matches, "", repo_id_arg)?;
    Ok(repo_id)
}

pub async fn open_sql<'a, T>(
    fb: FacebookInit,
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<T, Error>
where
    T: SqlConstructFromMetadataDatabaseConfig,
{
    let (_, config) = get_config(config_store, matches)?;
    let mysql_options = parse_mysql_options(matches);
    let readonly_storage = parse_readonly_storage(matches);
    T::with_metadata_database_config(
        fb,
        &config.storage_config.metadata,
        &mysql_options,
        readonly_storage.0,
    )
    .await
}

pub async fn open_source_sql<'a, T>(
    fb: FacebookInit,
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<T, Error>
where
    T: SqlConstructFromMetadataDatabaseConfig,
{
    let source_repo_id = get_source_repo_id(config_store, matches)?;
    let (_, config) = get_config_by_repoid(config_store, matches, source_repo_id)?;
    let mysql_options = parse_mysql_options(matches);
    let readonly_storage = parse_readonly_storage(matches);
    T::with_metadata_database_config(
        fb,
        &config.storage_config.metadata,
        &mysql_options,
        readonly_storage.0,
    )
    .await
}

/// Create a new `BlobRepo` -- for local instances, expect its contents to be empty.
#[inline]
pub fn create_repo<'a>(
    fb: FacebookInit,
    logger: &'a Logger,
    matches: &'a MononokeMatches<'a>,
) -> impl Future<Output = Result<BlobRepo, Error>> + 'a {
    open_repo_internal(
        fb,
        logger,
        matches,
        true,
        parse_caching(matches.as_ref()),
        None,
    )
}

/// Create a new `BlobRepo` -- for local instances, expect its contents to be empty.
/// Make sure that the opened repo has redaction disabled
#[inline]
pub fn create_repo_unredacted<'a>(
    fb: FacebookInit,
    logger: &'a Logger,
    matches: &'a MononokeMatches<'a>,
) -> impl Future<Output = Result<BlobRepo, Error>> + 'a {
    open_repo_internal(
        fb,
        logger,
        matches,
        true,
        parse_caching(matches.as_ref()),
        Some(Redaction::Disabled),
    )
}

/// Open an existing `BlobRepo` -- for local instances, expect contents to already be there.
#[inline]
pub fn open_repo<'a>(
    fb: FacebookInit,
    logger: &'a Logger,
    matches: &'a MononokeMatches<'a>,
) -> impl Future<Output = Result<BlobRepo, Error>> + 'a {
    open_repo_internal(
        fb,
        logger,
        matches,
        false,
        parse_caching(matches.as_ref()),
        None,
    )
}

/// Open an existing `BlobRepo` -- for local instances, expect contents to already be there.
/// Make sure that the opened repo has redaction disabled
#[inline]
pub fn open_repo_unredacted<'a>(
    fb: FacebookInit,
    logger: &'a Logger,
    matches: &'a MononokeMatches<'a>,
) -> impl Future<Output = Result<BlobRepo, Error>> + 'a {
    open_repo_internal(
        fb,
        logger,
        matches,
        false,
        parse_caching(matches.as_ref()),
        Some(Redaction::Disabled),
    )
}

/// Open an existing `BlobRepo` by ID -- for local instances, expect contents to already be there.
/// It useful when we need to open more than 1 mononoke repo based on command line arguments
#[inline]
pub fn open_repo_by_id<'a>(
    fb: FacebookInit,
    logger: &'a Logger,
    matches: &'a MononokeMatches<'a>,
    repo_id: RepositoryId,
) -> impl Future<Output = Result<BlobRepo, Error>> + 'a {
    open_repo_internal_with_repo_id(
        fb,
        logger,
        repo_id,
        matches,
        false, // use CreateStorage::ExistingOnly when creating blobstore
        parse_caching(matches.as_ref()),
        None, // do not override redaction config
    )
}

fn add_mysql_options_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name(MYSQL_MYROUTER_PORT)
            .long(MYSQL_MYROUTER_PORT)
            .help("Use MyRouter at this port")
            .takes_value(true),
    )
    .arg(
        Arg::with_name(MYSQL_MASTER_ONLY)
            .long(MYSQL_MASTER_ONLY)
            .help("Connect to MySQL master only")
            .takes_value(false),
    )
    .arg(
        Arg::with_name(MYSQL_USE_CLIENT)
            .long(MYSQL_USE_CLIENT)
            .help("Connect via Mysql client")
            .takes_value(false)
            .conflicts_with(MYSQL_MYROUTER_PORT),
    )
    // All the defaults for Mysql connection pool are derived from sql_ext::facebook::mysql
    // https://fburl.com/diffusion/n5isd68j
    // last synced on 17/12/2020
    .arg(
        Arg::with_name(MYSQL_POOL_LIMIT)
            .long(MYSQL_POOL_LIMIT)
            .help("Size of the connection pool")
            .takes_value(true)
            .default_value("10000"),
    )
    .arg(
        Arg::with_name(MYSQL_POOL_PER_KEY_LIMIT)
            .long(MYSQL_POOL_PER_KEY_LIMIT)
            .help("Mysql connection pool per key limit")
            .takes_value(true)
            .default_value("100"),
    )
    .arg(
        Arg::with_name(MYSQL_POOL_THREADS_NUM)
            .long(MYSQL_POOL_THREADS_NUM)
            .help("Number of threads in Mysql connection pool, i.e. number of real pools")
            .takes_value(true)
            .default_value("10"),
    )
    .arg(
        Arg::with_name(MYSQL_POOL_AGE_TIMEOUT)
            .long(MYSQL_POOL_AGE_TIMEOUT)
            .help("Mysql connection pool age timeout in millisecs")
            .takes_value(true)
            .default_value("60000"),
    )
    .arg(
        Arg::with_name(MYSQL_POOL_IDLE_TIMEOUT)
            .long(MYSQL_POOL_IDLE_TIMEOUT)
            .help("Mysql connection pool idle timeout in millisecs")
            .takes_value(true)
            .default_value("4000"),
    )
    .arg(
        Arg::with_name(MYSQL_CONN_OPEN_TIMEOUT)
            .long(MYSQL_CONN_OPEN_TIMEOUT)
            .help("Mysql connection open timeout in millisecs")
            .takes_value(true)
            .default_value("3000"),
    )
    .arg(
        Arg::with_name(MYSQL_MAX_QUERY_TIME)
            .long(MYSQL_MAX_QUERY_TIME)
            .help("Mysql query time limit in millisecs")
            .takes_value(true)
            .default_value("10000"),
    )
}

pub(crate) fn bool_as_str(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

pub(crate) const BOOL_VALUES: &[&str] = &["false", "true"];

pub fn add_mcrouter_args<'a, 'b>(app: MononokeClapApp<'a, 'b>) -> MononokeClapApp<'a, 'b> {
    app.arg(
        Arg::with_name(ENABLE_MCROUTER)
            .long(ENABLE_MCROUTER)
            .help("Use local McRouter for rate limits")
            .takes_value(false),
    )
}

fn add_fb303_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.args_from_usage(r"--fb303-thrift-port=[PORT]    'port for fb303 service'")
}

fn add_disabled_hooks_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("disabled-hooks")
            .long("disable-hook")
            .help("Disable a hook. Pass this argument multiple times to disable multiple hooks.")
            .multiple(true)
            .number_of_values(1)
            .takes_value(true),
    )
}

fn add_shutdown_timeout_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("shutdown-grace-period")
            .long("shutdown-grace-period")
            .help(
                "Number of seconds to wait after receiving a shutdown signal before shutting down.",
            )
            .takes_value(true)
            .required(false)
            .default_value("0"),
    )
    .arg(
        Arg::with_name("shutdown-timeout")
            .long("shutdown-timeout")
            .help("Number of seconds to wait for requests to complete during shutdown.")
            .takes_value(true)
            .required(false)
            .default_value("10"),
    )
}

pub fn get_shutdown_grace_period<'a>(matches: &MononokeMatches<'a>) -> Result<Duration> {
    let seconds = matches
        .value_of("shutdown-grace-period")
        .ok_or(Error::msg("shutdown-grace-period must be specified"))?
        .parse()
        .map_err(Error::from)?;
    Ok(Duration::from_secs(seconds))
}

pub fn get_shutdown_timeout<'a>(matches: &MononokeMatches<'a>) -> Result<Duration> {
    let seconds = matches
        .value_of("shutdown-timeout")
        .ok_or(Error::msg("shutdown-timeout must be specified"))?
        .parse()
        .map_err(Error::from)?;
    Ok(Duration::from_secs(seconds))
}

fn add_scuba_logging_args<'a, 'b>(app: App<'a, 'b>, has_default: bool) -> App<'a, 'b> {
    let mut app = app
        .arg(
            Arg::with_name("scuba-dataset")
                .long("scuba-dataset")
                .takes_value(true)
                .help("The name of the scuba dataset to log to"),
        )
        .arg(
            Arg::with_name("scuba-log-file")
                .long("scuba-log-file")
                .takes_value(true)
                .help("A log file to write Scuba logs to (primarily useful in testing)"),
        );

    if has_default {
        app = app.arg(
            Arg::with_name("no-default-scuba-dataset")
                .long("no-default-scuba-dataset")
                .takes_value(false)
                .help("Do not to the default scuba dataset for this app"),
        )
    }

    app
}

pub fn get_scuba_sample_builder<'a>(
    fb: FacebookInit,
    matches: &'a MononokeMatches<'a>,
    logger: &'a Logger,
) -> Result<MononokeScubaSampleBuilder> {
    let octx = init_observability_context(fb, matches, logger)?.clone();
    let mut scuba_logger = if let Some(scuba_dataset) = matches.value_of("scuba-dataset") {
        MononokeScubaSampleBuilder::new(fb, scuba_dataset)
    } else if let Some(default_scuba_dataset) = matches.app_data.default_scuba_dataset.as_ref() {
        if matches.is_present("no-default-scuba-dataset") {
            MononokeScubaSampleBuilder::with_discard()
        } else {
            MononokeScubaSampleBuilder::new(fb, default_scuba_dataset)
        }
    } else {
        MononokeScubaSampleBuilder::with_discard()
    };
    if let Some(scuba_log_file) = matches.value_of("scuba-log-file") {
        scuba_logger = scuba_logger.with_log_file(scuba_log_file)?;
    }
    let scuba_logger = scuba_logger
        .with_observability_context(octx)
        .with_seq("seq");
    Ok(scuba_logger)
}

pub fn add_scribe_logging_args<'a, 'b>(app: MononokeClapApp<'a, 'b>) -> MononokeClapApp<'a, 'b> {
    app.arg(
        Arg::with_name("scribe-logging-directory")
            .long("scribe-logging-directory")
            .takes_value(true)
            .help("Filesystem directory where to log all scribe writes"),
    )
}

pub fn get_scribe<'a>(fb: FacebookInit, matches: &MononokeMatches<'a>) -> Result<Scribe> {
    match matches.value_of("scribe-logging-directory") {
        Some(dir) => Ok(Scribe::new_to_file(PathBuf::from(dir))),
        None => Ok(Scribe::new(fb)),
    }
}

pub fn get_config_path<'a>(matches: &'a MononokeMatches<'a>) -> Result<&'a str> {
    matches
        .value_of(CONFIG_PATH)
        .ok_or(Error::msg(format!("{} must be specified", CONFIG_PATH)))
}

pub fn load_repo_configs<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<RepoConfigs> {
    metaconfig_parser::load_repo_configs(get_config_path(matches)?, config_store)
}

pub fn load_common_config<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<CommonConfig> {
    metaconfig_parser::load_common_config(get_config_path(matches)?, config_store)
}

pub fn load_storage_configs<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<StorageConfigs> {
    metaconfig_parser::load_storage_configs(get_config_path(matches)?, config_store)
}

pub fn get_config<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
) -> Result<(String, RepoConfig)> {
    let repo_id = get_repo_id(config_store, matches)?;
    get_config_by_repoid(config_store, matches, repo_id)
}

pub fn get_config_by_repoid<'a>(
    config_store: &ConfigStore,
    matches: &'a MononokeMatches<'a>,
    repo_id: RepositoryId,
) -> Result<(String, RepoConfig)> {
    let configs = load_repo_configs(config_store, matches)?;
    configs
        .get_repo_config(repo_id)
        .ok_or_else(|| format_err!("unknown repoid {:?}", repo_id))
        .map(|(name, config)| (name.clone(), config.clone()))
}

async fn open_repo_internal(
    fb: FacebookInit,
    logger: &Logger,
    matches: &MononokeMatches<'_>,
    create: bool,
    caching: Caching,
    redaction_override: Option<Redaction>,
) -> Result<BlobRepo, Error> {
    let config_store = init_config_store(fb, logger, matches)?;
    let repo_id = get_repo_id(config_store, matches)?;
    open_repo_internal_with_repo_id(
        fb,
        logger,
        repo_id,
        matches,
        create,
        caching,
        redaction_override,
    )
    .await
}

async fn open_repo_internal_with_repo_id(
    fb: FacebookInit,
    logger: &Logger,
    repo_id: RepositoryId,
    matches: &MononokeMatches<'_>,
    create: bool,
    caching: Caching,
    redaction_override: Option<Redaction>,
) -> Result<BlobRepo, Error> {
    let config_store = init_config_store(fb, logger, matches)?;
    let common_config = load_common_config(config_store, &matches)?;
    let (reponame, config) = get_config_by_repoid(config_store, matches, repo_id)?;
    info!(logger, "using repo \"{}\" repoid {:?}", reponame, repo_id);
    match &config.storage_config.blobstore {
        BlobConfig::Files { path } | BlobConfig::Sqlite { path } => {
            let create = if create {
                // Many path repos can share one blobstore, so allow store to exist or create it.
                CreateStorage::ExistingOrCreate
            } else {
                CreateStorage::ExistingOnly
            };
            setup_repo_dir(path, create)?;
        }
        _ => {}
    };

    let mysql_options = parse_mysql_options(matches);
    let blobstore_options = parse_blobstore_options(matches)?;
    let readonly_storage = parse_readonly_storage(matches);

    let mut builder = BlobrepoBuilder::new(
        fb,
        reponame,
        &config,
        &mysql_options,
        caching,
        common_config.censored_scuba_params,
        readonly_storage,
        blobstore_options,
        &logger,
        config_store,
    );
    if let Some(redaction_override) = redaction_override {
        builder.set_redaction(redaction_override);
    }
    builder.build().await
}

pub async fn open_repo_with_repo_id<'a>(
    fb: FacebookInit,
    logger: &Logger,
    repo_id: RepositoryId,
    matches: &'a MononokeMatches<'a>,
) -> Result<BlobRepo, Error> {
    open_repo_internal_with_repo_id(
        fb,
        logger,
        repo_id,
        matches,
        false,
        parse_caching(matches.as_ref()),
        None,
    )
    .await
}

pub fn parse_readonly_storage<'a>(matches: &MononokeMatches<'a>) -> ReadOnlyStorage {
    if matches.is_present(READONLY_STORAGE_OLD_ARG) {
        ReadOnlyStorage(true)
    } else {
        ReadOnlyStorage(
            matches
                .value_of(READONLY_STORAGE_NEW_ARG)
                .map_or(false, |v| {
                    v.parse().unwrap_or_else(|_| {
                        panic!("Provided {} is not bool", READONLY_STORAGE_NEW_ARG)
                    })
                }),
        )
    }
}

pub fn get_global_mysql_connection_pool<'a>(matches: &MononokeMatches<'a>) -> SharedConnectionPool {
    matches.app_data.global_mysql_connection_pool.clone()
}

fn parse_mysql_pool_options<'a>(matches: &MononokeMatches<'a>) -> PoolConfig {
    let size: usize = matches
        .value_of(MYSQL_POOL_LIMIT)
        .map(|v| v.parse().expect("Provided mysql-pool-limit is not usize"))
        .expect("A default is set, should never be None");
    let threads_num: i32 = matches
        .value_of(MYSQL_POOL_THREADS_NUM)
        .map(|v| {
            v.parse()
                .expect("Provided mysql-pool-threads-num is not i32")
        })
        .expect("A default is set, should never be None");
    let per_key_limit: u64 = matches
        .value_of(MYSQL_POOL_PER_KEY_LIMIT)
        .map(|v| {
            v.parse()
                .expect("Provided mysql-pool-per-key-limit is not u64")
        })
        .expect("A default is set, should never be None");
    let conn_age_timeout: u64 = matches
        .value_of(MYSQL_POOL_AGE_TIMEOUT)
        .map(|v| {
            v.parse()
                .expect("Provided mysql-pool-age-timeout is not u64")
        })
        .expect("A default is set, should never be None");
    let conn_idle_timeout: u64 = matches
        .value_of(MYSQL_POOL_IDLE_TIMEOUT)
        .map(|v| v.parse().expect("Provided mysql-pool-limit is not usize"))
        .expect("A default is set, should never be None");
    let conn_open_timeout: u64 = matches
        .value_of(MYSQL_CONN_OPEN_TIMEOUT)
        .map(|v| {
            v.parse()
                .expect("Provided mysql-conn-open-timeout is not u64")
        })
        .expect("A default is set, should never be None");
    let max_query_time: Duration = Duration::from_millis(
        matches
            .value_of(MYSQL_MAX_QUERY_TIME)
            .map(|v| {
                v.parse()
                    .expect("Provided mysql-query-time-limit is not u64")
            })
            .expect("A default is set, should never be None"),
    );

    PoolConfig::new(
        size,
        threads_num,
        per_key_limit,
        conn_age_timeout,
        conn_idle_timeout,
        conn_open_timeout,
        max_query_time,
    )
}

pub fn parse_mysql_options<'a>(matches: &MononokeMatches<'a>) -> MysqlOptions {
    let connection_type = if let Some(port) = matches.value_of(MYSQL_MYROUTER_PORT) {
        let port = port
            .parse::<u16>()
            .expect("Provided --myrouter-port is not u16");
        MysqlConnectionType::Myrouter(port)
    } else if matches.is_present(MYSQL_USE_CLIENT) {
        let pool = get_global_mysql_connection_pool(matches);
        let pool_config = parse_mysql_pool_options(matches);

        MysqlConnectionType::Mysql(pool, pool_config)
    } else {
        MysqlConnectionType::RawXDB
    };

    let master_only = matches.is_present(MYSQL_MASTER_ONLY);

    MysqlOptions {
        connection_type,
        master_only,
    }
}

pub fn parse_blobstore_options(matches: &MononokeMatches) -> Result<BlobstoreOptions, Error> {
    let read_qps: Option<NonZeroU32> = matches
        .value_of(READ_QPS_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided qps is not u32")?;

    let write_qps: Option<NonZeroU32> = matches
        .value_of(WRITE_QPS_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided qps is not u32")?;

    let read_bytes: Option<NonZeroUsize> = matches
        .value_of(READ_BYTES_ARG)
        .map(|v| v.parse().expect("Provided Bytes/s is not usize"));

    let write_bytes: Option<NonZeroUsize> = matches
        .value_of(WRITE_BYTES_ARG)
        .map(|v| v.parse().expect("Provided Bytes/s is not usize"));

    let read_burst_bytes: Option<NonZeroUsize> = matches
        .value_of(READ_BURST_BYTES_ARG)
        .map(|v| v.parse().expect("Provided Bytes/s is not usize"));

    let write_burst_bytes: Option<NonZeroUsize> = matches
        .value_of(WRITE_BURST_BYTES_ARG)
        .map(|v| v.parse().expect("Provided Bytes/s is not usize"));

    let bytes_min_count: Option<NonZeroUsize> = matches
        .value_of(BLOBSTORE_BYTES_MIN_THROTTLE_ARG)
        .map(|v| v.parse().expect("Provided Bytes/s is not usize"));

    let read_chaos: Option<NonZeroU32> = matches
        .value_of(READ_CHAOS_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided chaos is not u32")?;

    let write_chaos: Option<NonZeroU32> = matches
        .value_of(WRITE_CHAOS_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided chaos is not u32")?;

    let manifold_api_key: Option<String> = matches
        .value_of(MANIFOLD_API_KEY_ARG)
        .map(|api_key| api_key.to_string());

    let manifold_use_cpp_client: bool = matches
        .value_of(MANIFOLD_USE_CPP_CLIENT_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided manifold-use-cpp-client is not bool")?
        .ok_or_else(|| format_err!("A default is set, should never be None"))?;

    let write_zstd_level: Option<i32> = matches
        .value_of(WRITE_ZSTD_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided Zstd compression level is not i32")?;

    let attempt_zstd: bool = matches
        .value_of(CACHELIB_ATTEMPT_ZSTD_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided blobstore-cachelib-attempt-zstd is not bool")?
        .ok_or_else(|| format_err!("A default is set, should never be None"))?;

    let blobstore_put_behaviour: Option<PutBehaviour> = matches
        .value_of(BLOBSTORE_PUT_BEHAVIOUR_ARG)
        .map(|v| v.parse())
        .transpose()
        .context("Provided blobstore-put-behaviour is not PutBehaviour")?;

    let blobstore_options = BlobstoreOptions::new(
        ChaosOptions::new(read_chaos, write_chaos),
        ThrottleOptions {
            read_qps,
            write_qps,
            read_bytes,
            write_bytes,
            read_burst_bytes,
            write_burst_bytes,
            bytes_min_count,
        },
        manifold_api_key,
        manifold_use_cpp_client,
        PackOptions::new(write_zstd_level),
        CachelibBlobstoreOptions::new_lazy(Some(attempt_zstd)),
        blobstore_put_behaviour,
    );

    let blobstore_options = if matches.arg_types.contains(&ArgType::Scrub) {
        let scrub_action = matches
            .value_of(BLOBSTORE_SCRUB_ACTION_ARG)
            .map(ScrubAction::from_str)
            .transpose()?;
        let scrub_grace = matches
            .value_of(BLOBSTORE_SCRUB_GRACE_ARG)
            .map(u64::from_str)
            .transpose()?;
        blobstore_options
            .with_scrub_action(scrub_action)
            .with_scrub_grace(scrub_grace)
    } else {
        blobstore_options
    };

    Ok(blobstore_options)
}

pub fn maybe_enable_mcrouter<'a>(fb: FacebookInit, matches: &MononokeMatches<'a>) {
    if matches.is_present(ENABLE_MCROUTER) {
        #[cfg(fbcode_build)]
        {
            ::ratelim::use_proxy_if_available(fb);
        }
        #[cfg(not(fbcode_build))]
        {
            let _ = fb;
            unimplemented!(
                "Passed --{}, but it is supported only for fbcode builds",
                ENABLE_MCROUTER
            );
        }
    }
}

pub fn get_usize_opt<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str) -> Option<usize> {
    matches.borrow().value_of(key).map(|val| {
        val.parse::<usize>()
            .expect(&format!("{} must be integer", key))
    })
}

#[inline]
pub fn get_usize<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str, default: usize) -> usize {
    get_usize_opt(matches, key).unwrap_or(default)
}

#[inline]
pub fn get_u64<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str, default: u64) -> u64 {
    get_u64_opt(matches, key).unwrap_or(default)
}

#[inline]
pub fn get_and_parse_opt<'a, T: ::std::str::FromStr, M: Borrow<ArgMatches<'a>>>(
    matches: &M,
    key: &str,
) -> Option<T>
where
    <T as std::str::FromStr>::Err: std::fmt::Debug,
{
    matches
        .borrow()
        .value_of(key)
        .map(|val| val.parse::<T>().expect(&format!("{} - invalid value", key)))
}

#[inline]
pub fn get_and_parse<'a, T: ::std::str::FromStr, M: Borrow<ArgMatches<'a>>>(
    matches: &M,
    key: &str,
    default: T,
) -> T
where
    <T as std::str::FromStr>::Err: std::fmt::Debug,
{
    get_and_parse_opt(matches, key).unwrap_or(default)
}

#[inline]
pub fn get_u64_opt<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str) -> Option<u64> {
    matches.borrow().value_of(key).map(|val| {
        val.parse::<u64>()
            .expect(&format!("{} must be integer", key))
    })
}

#[inline]
pub fn get_i32_opt<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str) -> Option<i32> {
    matches.borrow().value_of(key).map(|val| {
        val.parse::<i32>()
            .expect(&format!("{} must be integer", key))
    })
}

#[inline]
pub fn get_i32<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str, default: i32) -> i32 {
    get_i32_opt(matches, key).unwrap_or(default)
}

#[inline]
pub fn get_i64_opt<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str) -> Option<i64> {
    matches.borrow().value_of(key).map(|val| {
        val.parse::<i64>()
            .expect(&format!("{} must be integer", key))
    })
}

pub fn get_bool_opt<'a>(matches: &impl Borrow<ArgMatches<'a>>, key: &str) -> Option<bool> {
    matches.borrow().value_of(key).map(|val| {
        val.parse::<bool>()
            .unwrap_or_else(|_| panic!("{} must be bool", key))
    })
}

pub fn parse_disabled_hooks_with_repo_prefix<'a>(
    matches: &'a MononokeMatches<'a>,
    logger: &Logger,
) -> Result<HashMap<String, HashSet<String>>, Error> {
    let disabled_hooks = matches
        .values_of("disabled-hooks")
        .map(|m| m.collect())
        .unwrap_or(vec![]);

    let mut res = HashMap::new();
    for repohook in disabled_hooks {
        let repohook: Vec<_> = repohook.splitn(2, ":").collect();
        let repo = repohook.get(0);
        let hook = repohook.get(1);

        let (repo, hook) =
            repo.and_then(|repo| hook.map(|hook| (repo, hook)))
                .ok_or(format_err!(
                    "invalid format of disabled hook, should be 'REPONAME:HOOKNAME'"
                ))?;
        res.entry(repo.to_string())
            .or_insert(HashSet::new())
            .insert(hook.to_string());
    }
    if !res.is_empty() {
        warn!(logger, "The following Hooks were disabled: {:?}", res);
    }
    Ok(res)
}

pub fn parse_disabled_hooks_no_repo_prefix<'a>(
    matches: &'a MononokeMatches<'a>,
    logger: &Logger,
) -> HashSet<String> {
    let disabled_hooks: HashSet<String> = matches
        .values_of("disabled-hooks")
        .map(|m| m.collect())
        .unwrap_or(vec![])
        .into_iter()
        .map(|s| s.to_string())
        .collect();

    if !disabled_hooks.is_empty() {
        warn!(
            logger,
            "The following Hooks were disabled: {:?}", disabled_hooks
        );
    }

    disabled_hooks
}

pub fn init_mononoke<'a>(
    fb: FacebookInit,
    matches: &'a MononokeMatches<'a>,
) -> Result<(Caching, Logger, tokio::runtime::Runtime)> {
    init_mononoke_with_cache_settings(fb, matches, CachelibSettings::default())
}

// TODO(ahornby) move into MononokeMatches when all init_mononoke call sites changed to call MononokeMatches::init_mononoke()
fn init_mononoke_with_cache_settings<'a>(
    fb: FacebookInit,
    matches: &'a MononokeMatches<'a>,
    cachelib_settings: CachelibSettings,
) -> Result<(Caching, Logger, tokio::runtime::Runtime)> {
    let logger = init_logging(fb, matches)?;

    debug!(logger, "Initialising cachelib...");
    let caching = parse_and_init_cachelib(fb, matches.as_ref(), cachelib_settings);
    debug!(logger, "Initialising runtime...");
    let runtime = init_runtime(matches)?;
    init_tunables(fb, matches, logger.clone())?;

    Ok((caching, logger, runtime))
}

pub fn init_tunables<'a>(
    fb: FacebookInit,
    matches: &'a MononokeMatches<'a>,
    logger: Logger,
) -> Result<()> {
    if matches.is_present(DISABLE_TUNABLES) {
        debug!(logger, "Tunables are disabled");
        return Ok(());
    }

    let config_store = init_config_store(fb, &logger, matches)?;

    let tunables_spec = matches
        .value_of(TUNABLES_CONFIG)
        .unwrap_or(DEFAULT_TUNABLES_PATH);

    let config_handle = get_config_handle(config_store, &logger, Some(tunables_spec))?;

    init_tunables_worker(logger, config_handle)
}
/// Initialize a new `tokio::runtime::Runtime` with thread number parsed from the CLI
pub fn init_runtime(matches: &MononokeMatches) -> io::Result<tokio::runtime::Runtime> {
    let core_threads = get_usize_opt(matches, RUNTIME_THREADS);
    create_runtime(None, core_threads)
}

/// Extract a ConfigHandle<T> from a source_spec str that has one ofthe folowing formats:
/// - configerator:PATH
/// - file:PATH
/// - default
/// NB: Outside tests, using file:PATH is not recommended because it is inefficient - instead
/// use a local configerator path and configerator:PATH
pub fn get_config_handle<T>(
    config_store: &ConfigStore,
    logger: &Logger,
    source_spec: Option<&str>,
) -> Result<ConfigHandle<T>, Error>
where
    T: Default + Send + Sync + 'static + serde::de::DeserializeOwned,
{
    match source_spec {
        Some(source_spec) => {
            // NOTE: This means we don't support file paths with ":" in them, but it also means we can
            // add other options after the first ":" later if we want.
            let mut iter = source_spec.split(":");

            // NOTE: We match None as the last element to make sure the input doesn't contain
            // disallowed trailing parts.
            match (iter.next(), iter.next(), iter.next()) {
                (Some("configerator"), Some(source), None) => {
                    config_store.get_config_handle(source.to_string())
                }
                (Some("file"), Some(file), None) => ConfigStore::file(
                    logger.clone(),
                    PathBuf::new(),
                    String::new(),
                    Duration::from_secs(1),
                )
                .get_config_handle(file.to_string()),
                (Some("default"), None, None) => Ok(ConfigHandle::default()),
                _ => Err(format_err!("Invalid configuration spec: {:?}", source_spec)),
            }
        }
        None => Ok(ConfigHandle::default()),
    }
}

static CONFIGERATOR: OnceCell<ConfigStore> = OnceCell::new();

static OBSERVABILITY_CONTEXT: OnceCell<ObservabilityContext> = OnceCell::new();

pub fn init_observability_context<'a>(
    fb: FacebookInit,
    matches: &'a MononokeMatches<'a>,
    root_log: impl Into<Option<&'a Logger>>,
) -> Result<&'static ObservabilityContext, Error> {
    OBSERVABILITY_CONTEXT.get_or_try_init(|| match matches.value_of(WITH_DYNAMIC_OBSERVABILITY) {
        Some("true") => {
            let config_store = init_config_store(fb, root_log, matches)?;
            Ok(ObservabilityContext::new(config_store)?)
        }
        Some("false") | None => Ok(ObservabilityContext::new_static(get_log_level(matches))),
        Some(other) => panic!(
            "Unexpected --{} value: {}",
            WITH_DYNAMIC_OBSERVABILITY, other
        ),
    })
}

pub fn init_config_store<'a>(
    fb: FacebookInit,
    root_log: impl Into<Option<&'a Logger>>,
    matches: &'a MononokeMatches<'a>,
) -> Result<&'static ConfigStore, Error> {
    CONFIGERATOR.get_or_try_init(|| {
        let local_configerator_path = matches.value_of(LOCAL_CONFIGERATOR_PATH_ARG);
        let crypto_regex = matches.values_of(CRYPTO_PATH_REGEX_ARG).map_or(
            vec![
                (
                    "scm/mononoke/tunables/.*".to_string(),
                    CRYPTO_PROJECT.to_string(),
                ),
                (
                    "scm/mononoke/repos/.*".to_string(),
                    CRYPTO_PROJECT.to_string(),
                ),
            ],
            |it| {
                it.map(|regex| (regex.to_string(), CRYPTO_PROJECT.to_string()))
                    .collect()
            },
        );
        match local_configerator_path {
            // A local configerator path wins
            Some(path) => Ok(ConfigStore::file(
                root_log.into().cloned(),
                PathBuf::from(path),
                String::new(),
                CONFIGERATOR_POLL_INTERVAL,
            )),
            // Prod instances do have network configerator, with signature checks
            None => ConfigStore::regex_signed_configerator(
                fb,
                root_log.into().cloned(),
                crypto_regex,
                CONFIGERATOR_POLL_INTERVAL,
                CONFIGERATOR_REFRESH_TIMEOUT,
            ),
        }
    })
}
