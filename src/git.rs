use crate::Error;
use chrono::{
    offset::{TimeZone, Utc},
    DateTime,
};
use git2::{ObjectType, Repository, Sort};
use semver::Version;
use std::convert::{TryFrom, TryInto};

/// A commit owning all the relevant data to be used in Jilu.
#[derive(Debug)]
pub struct Commit {
    pub(crate) id: String,
    pub(crate) short_id: String,
    pub(crate) message: String,
    pub(crate) time: DateTime<Utc>,
    pub(crate) author: Signature,
    pub(crate) committer: Signature,
}

/// A tag owning all the relevant data to be used in Jilu.
#[derive(Debug)]
pub struct Tag {
    pub(crate) id: String,
    pub(crate) message: Option<String>,
    pub(crate) name: String,
    pub(crate) version: Version,
    pub(crate) tagger: Option<Signature>,
    pub(crate) commit: Commit,
}

/// A signature owning all the relevant data to be used in Jilu.
#[derive(Debug)]
pub struct Signature {
    pub(crate) email: String,
    pub(crate) name: String,
    pub(crate) time: DateTime<Utc>,
}

/// Fetch all Git commits to be presented in the change log.
///
/// This function walks over a tree in the Git repository, and converts all Git
/// commits into our own `Commit` wrapper, for ease of use and testing.
///
/// Any commits that do not conform to our expected layout will be ignored
/// without returning an error (but with an optional log line to explain why the
/// commit was ignored), to allow the application to be used in repositories
/// where not all commits adhere to the expected format.
///
/// Any unexpected error is still bubbled up to the callee.
pub fn commits(repo: &Repository) -> Result<Vec<Commit>, Error> {
    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    walk.simplify_first_parent()?;
    walk.set_sorting(Sort::REVERSE | Sort::TOPOLOGICAL)?;

    // walk the tree of commits, keeping track of the object ID throughout the
    // process to be able to point towards any commits causing an error.
    walk.map(|result| {
        result.map_err(|err| (None, err.into())).and_then(|oid| {
            repo.find_commit(oid)
                .map_err(Into::into)
                .and_then(TryInto::try_into)
                .map_err(|err| (Some(oid), err))
        })
    })
    .filter_map(|result| match result {
        Err((oid, err)) => match err {
            // Any badly formatted commit is skipped.
            Error::Utf8Error => {
                // TODO: debug logging
                eprintln!(
                    "[debug] ignoring bad commit {}: {}",
                    oid.as_ref().map(ToString::to_string).unwrap_or_default(),
                    err
                );
                None
            }
            // All non-defined errors above are considered to be breaking and
            // are bubbled up to the callee.
            _ => Some(Err(err)),
        },
        Ok(commit) => Some(Ok(commit)),
    })
    .collect()
}

/// Fetch all Git tags to be used as release tags in the change log.
///
/// This function fetches all Git tags, and converts them into our own `Tag`
/// wrapper, for ease of use and testing.
///
/// Any tags that do not conform to our expected layout will be ignored without
/// returning an error (but with an optional log line to explain why the tag was
/// ignored), to allow the application to be used in repositories where not all
/// tags adhere to the expected format.
///
/// Any unexpected error is still bubbled up to the callee.
pub fn tags(repo: &Repository) -> Result<Vec<Tag>, Error> {
    let mut tags: Vec<Tag> = repo
        .tag_names(None)?
        .into_iter()
        .map(|string| {
            string.ok_or((None, Error::Utf8Error)).and_then(|name| {
                repo.revparse_single(name)
                    .map_err(Into::into)
                    .and_then(|object| {
                        match object.kind() {
                            // annotated tag
                            Some(ObjectType::Tag) => object
                                .into_tag()
                                .map_err(|_| Error::InvalidTag)
                                .and_then(TryInto::try_into),
                            // lightweight tag
                            Some(ObjectType::Commit) => object
                                .into_commit()
                                .map_err(|_| Error::InvalidTag)
                                .and_then(|c| (name, c).try_into()),
                            _ => unreachable!(),
                        }
                    })
                    .map_err(|err| (Some(name), err))
            })
        })
        .filter_map(|result: Result<Tag, _>| match result {
            Err((name, err)) => match err {
                // Any badly formatted tag is skipped.
                Error::Utf8Error | Error::SemVer(_) => {
                    // TODO: debug logging
                    eprintln!(
                        "[debug] ignoring bad tag {}: {}",
                        name.unwrap_or_default(),
                        err
                    );
                    None
                }

                // All non-defined errors above are considered to be breaking
                // and are bubbled up to the callee.
                _ => Some(Err(err)),
            },
            Ok(tag) => Some(Ok(tag)),
        })
        .collect::<Result<Vec<_>, _>>()?;

    tags.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(tags)
}

impl TryFrom<git2::Commit<'_>> for Commit {
    type Error = Error;

    fn try_from(commit: git2::Commit<'_>) -> Result<Self, Error> {
        Ok(Self {
            id: commit.id().to_string(),
            short_id: commit
                .as_object()
                .short_id()?
                .as_str()
                .ok_or(Error::Utf8Error)?
                .to_owned(),
            message: commit
                .message()
                .ok_or(Error::Utf8Error)?
                .trim_end()
                .to_owned(),
            author: commit.author().try_into()?,
            committer: commit.committer().try_into()?,
            time: Utc.timestamp(commit.time().seconds(), 0),
        })
    }
}

impl TryFrom<git2::Tag<'_>> for Tag {
    type Error = Error;

    fn try_from(tag: git2::Tag<'_>) -> Result<Self, Error> {
        if tag.target_type() != Some(ObjectType::Commit) {
            return Err(Error::InvalidTag);
        }

        let name = tag.name().ok_or(Error::Utf8Error)?.to_owned();
        let version = Version::parse(if name.starts_with('v') {
            &name[1..]
        } else {
            &name
        })?;

        Ok(Self {
            id: tag.id().to_string(),
            message: tag.message().map(str::to_owned),
            name,
            version,
            tagger: tag.tagger().map(TryInto::try_into).transpose()?,
            commit: tag
                .target()?
                .into_commit()
                .map_err(|_| git2::Error::from_str("tag does not point to commit"))?
                .try_into()?,
        })
    }
}

impl TryFrom<git2::Signature<'_>> for Signature {
    type Error = Error;

    fn try_from(signature: git2::Signature<'_>) -> Result<Self, Error> {
        Ok(Self {
            email: signature.email().ok_or(Error::Utf8Error)?.to_owned(),
            name: signature.name().ok_or(Error::Utf8Error)?.to_owned(),
            time: Utc.timestamp(signature.when().seconds(), 0),
        })
    }
}
impl TryFrom<(&str, git2::Commit<'_>)> for Tag {
    type Error = Error;

    fn try_from((name, commit): (&str, git2::Commit<'_>)) -> Result<Self, Error> {
        let version = Version::parse(if name.starts_with('v') {
            &name[1..]
        } else {
            &name
        })?;

        Ok(Self {
            id: commit.id().to_string(),
            message: None,
            name: name.to_owned(),
            version,
            tagger: Some(commit.author().try_into()?),
            commit: commit.try_into()?,
        })
    }
}
