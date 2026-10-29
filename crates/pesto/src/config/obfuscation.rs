//! Obfuscation modes and the wire-level policy each one implies.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Whether to obfuscate a post.
///
/// When enabled, both the subject line and the yEnc `name=` field are
/// randomised on the wire. The real filename is always preserved in the
/// generated NZB so that download clients can restore it correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ObfuscateMode {
    /// No obfuscation: the real file name appears in the subject and yEnc header.
    #[default]
    None,
    /// Private default: every article gets a fresh Subject and From while a
    /// physical file keeps one opaque yEnc name for client-safe assembly.
    ///
    /// `header-fragmented` remains accepted as a compatibility alias.
    #[serde(alias = "header-fragmented")]
    #[value(alias = "header-fragmented")]
    Full,
    /// Like `full`, but every file posted in the same run (archive parts and
    /// PAR2 volumes alike) shares one random prefix instead of each getting
    /// an independently-random name. The real names still stay off the wire;
    /// what changes is that Usenet indexers can once again recognise the
    /// PAR2 set and the content as one release, which plain `full` prevents
    /// (see GitHub issue #58). This trades away resistance to correlation by
    /// wire metadata for indexer compatibility — the opposite trade `article`
    /// makes — so it is a distinct mode rather than a `full` variant. The yEnc
    /// `name=` starts with that same shared prefix but adds its own random
    /// suffix rather than repeating the Subject verbatim (see `light` below
    /// for the variant that does repeat it).
    #[serde(rename = "full-shared")]
    FullShared,
    /// Like `full-shared` — one shared random prefix across every file in the
    /// release — but the yEnc `name=` is that prefixed string verbatim, the
    /// same as the Subject, instead of adding its own random suffix. This is
    /// `full-shared`'s behavior prior to `v0.6.1` (commit `bb9e3b2`), which
    /// decoupled Subject and yEnc name everywhere to close an exact-match
    /// fingerprint (Subject header == yEnc body name=) that identified posts
    /// made by this tool. Some indexers key their own grouping off that exact
    /// match, though (reportedly including NZBIndex), so `light` restores it
    /// for anyone who needs that over avoiding the fingerprint (see GitHub
    /// issue #106).
    Light,
    /// Legacy experimental mode: each article gets independent Subject, yEnc
    /// name and From identities. It deliberately remains hidden because
    /// multipart PAR2 cleanup is not compatible with conventional clients.
    #[serde(alias = "paranoid")]
    #[value(alias = "paranoid")]
    #[value(hide = true)]
    Article,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityScope {
    RealPath,
    Release,
    File,
    Article,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameRelation {
    Same,
    SharedPrefix,
    Independent,
}

/// The complete wire/privacy contract for an obfuscation mode. Keeping these
/// decisions together prevents posting, resume and PAR2 paths from drifting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObfuscationPolicy {
    pub subject_scope: IdentityScope,
    pub yenc_scope: IdentityScope,
    pub from_scope: IdentityScope,
    pub subject_yenc_relation: NameRelation,
    pub shared_release_prefix: bool,
    pub publish_par2_index: bool,
    pub allow_file_counter: bool,
}

impl ObfuscateMode {
    pub(crate) const fn policy(self) -> ObfuscationPolicy {
        match self {
            Self::None => ObfuscationPolicy {
                subject_scope: IdentityScope::RealPath,
                yenc_scope: IdentityScope::RealPath,
                from_scope: IdentityScope::Release,
                subject_yenc_relation: NameRelation::Same,
                shared_release_prefix: false,
                publish_par2_index: true,
                allow_file_counter: true,
            },
            Self::Light => ObfuscationPolicy {
                subject_scope: IdentityScope::Release,
                yenc_scope: IdentityScope::Release,
                from_scope: IdentityScope::Release,
                subject_yenc_relation: NameRelation::Same,
                shared_release_prefix: true,
                publish_par2_index: true,
                allow_file_counter: true,
            },
            Self::FullShared => ObfuscationPolicy {
                subject_scope: IdentityScope::Release,
                yenc_scope: IdentityScope::Release,
                from_scope: IdentityScope::Release,
                subject_yenc_relation: NameRelation::SharedPrefix,
                shared_release_prefix: true,
                publish_par2_index: true,
                allow_file_counter: true,
            },
            Self::Full => ObfuscationPolicy {
                subject_scope: IdentityScope::Article,
                yenc_scope: IdentityScope::File,
                from_scope: IdentityScope::Article,
                subject_yenc_relation: NameRelation::Independent,
                shared_release_prefix: false,
                publish_par2_index: false,
                allow_file_counter: false,
            },
            Self::Article => ObfuscationPolicy {
                subject_scope: IdentityScope::Article,
                yenc_scope: IdentityScope::Article,
                from_scope: IdentityScope::Article,
                subject_yenc_relation: NameRelation::Independent,
                shared_release_prefix: false,
                publish_par2_index: false,
                allow_file_counter: false,
            },
        }
    }
}
