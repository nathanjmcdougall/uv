use std::iter;

use pubgrub::Ranges;
use tracing::warn;

use uv_normalize::{ExtraName, GroupName, PackageName};
use uv_pep440::{Version, VersionSpecifiers};
use uv_pypi_types::{
    ParsedArchiveUrl, ParsedDirectoryUrl, ParsedGitUrl, ParsedPathUrl, ParsedUrl, Requirement,
    RequirementSource, VerbatimParsedUrl,
};

use crate::pubgrub::{PubGrubPackage, PubGrubPackageInner};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PubGrubDependency {
    pub(crate) package: PubGrubPackage,
    pub(crate) version: Ranges<Version>,

    /// The original version specifiers from the requirement.
    pub(crate) specifier: Option<VersionSpecifiers>,

    /// This field is set if the [`Requirement`] had a URL. We still use a URL from [`Urls`]
    /// even if this field is None where there is an override with a URL or there is a different
    /// requirement or constraint for the same package that has a URL.
    pub(crate) url: Option<VerbatimParsedUrl>,
}

impl PubGrubDependency {
    pub(crate) fn from_requirement<'a>(
        requirement: &'a Requirement,
        dev: Option<&'a GroupName>,
        source_name: Option<&'a PackageName>,
    ) -> impl Iterator<Item = Self> + 'a {
        // Add the package, plus any extra variants.
        iter::once(None)
            .chain(requirement.extras.clone().into_iter().map(Some))
            .map(|extra| PubGrubRequirement::from_requirement(requirement, extra))
            .filter_map(move |requirement| {
                let PubGrubRequirement {
                    package,
                    version,
                    specifier,
                    url,
                } = requirement;
                match &*package {
                    PubGrubPackageInner::Package { name, .. } => {
                        // Detect self-dependencies.
                        if dev.is_none() {
                            if source_name.is_some_and(|source_name| source_name == name) {
                                warn!("{name} has a dependency on itself");
                                return None;
                            }
                        }

                        Some(PubGrubDependency {
                            package: package.clone(),
                            version: version.clone(),
                            specifier,
                            url,
                        })
                    }
                    PubGrubPackageInner::Marker { .. } => Some(PubGrubDependency {
                        package: package.clone(),
                        version: version.clone(),
                        specifier,
                        url,
                    }),
                    PubGrubPackageInner::Extra { name, .. } => {
                        // Detect self-dependencies.
                        if dev.is_none() {
                            debug_assert!(
                                source_name.is_none_or(|source_name| source_name != name),
                                "extras not flattened for {name}"
                            );
                        }
                        Some(PubGrubDependency {
                            package: package.clone(),
                            version: version.clone(),
                            specifier,
                            url,
                        })
                    }
                    _ => None,
                }
            })
    }
}

/// A PubGrub-compatible package and version range.
#[derive(Debug, Clone)]
pub(crate) struct PubGrubRequirement {
    pub(crate) package: PubGrubPackage,
    pub(crate) version: Ranges<Version>,
    pub(crate) specifier: Option<VersionSpecifiers>,
    pub(crate) url: Option<VerbatimParsedUrl>,
}

impl PubGrubRequirement {
    /// Convert a [`Requirement`] to a PubGrub-compatible package and range, while returning the URL
    /// on the [`Requirement`], if any.
    pub(crate) fn from_requirement(requirement: &Requirement, extra: Option<ExtraName>) -> Self {
        let (verbatim_url, parsed_url) = match &requirement.source {
            RequirementSource::Registry { specifier, .. } => {
                return Self::from_registry_requirement(specifier, extra, requirement);
            }
            RequirementSource::Url {
                subdirectory,
                location,
                ext,
                url,
            } => {
                let parsed_url = ParsedUrl::Archive(ParsedArchiveUrl::from_source(
                    location.clone(),
                    subdirectory.clone(),
                    *ext,
                ));
                (url, parsed_url)
            }
            RequirementSource::Git {
                repository,
                reference,
                precise,
                url,
                subdirectory,
            } => {
                let parsed_url = ParsedUrl::Git(ParsedGitUrl::from_source(
                    repository.clone(),
                    reference.clone(),
                    *precise,
                    subdirectory.clone(),
                ));
                (url, parsed_url)
            }
            RequirementSource::Path {
                ext,
                url,
                install_path,
            } => {
                let parsed_url = ParsedUrl::Path(ParsedPathUrl::from_source(
                    install_path.clone(),
                    *ext,
                    url.to_url(),
                ));
                (url, parsed_url)
            }
            RequirementSource::Directory {
                editable,
                r#virtual,
                url,
                install_path,
            } => {
                let parsed_url = ParsedUrl::Directory(ParsedDirectoryUrl::from_source(
                    install_path.clone(),
                    *editable,
                    *r#virtual,
                    url.to_url(),
                ));
                (url, parsed_url)
            }
        };

        Self {
            package: PubGrubPackage::from_package(
                requirement.name.clone(),
                extra,
                requirement.marker.clone(),
            ),
            version: Ranges::full(),
            specifier: None,
            url: Some(VerbatimParsedUrl {
                parsed_url,
                verbatim: verbatim_url.clone(),
            }),
        }
    }

    fn from_registry_requirement(
        specifier: &VersionSpecifiers,
        extra: Option<ExtraName>,
        requirement: &Requirement,
    ) -> PubGrubRequirement {
        Self {
            package: PubGrubPackage::from_package(
                requirement.name.clone(),
                extra,
                requirement.marker.clone(),
            ),
            specifier: Some(specifier.clone()),
            url: None,
            version: Ranges::from(specifier.clone()),
        }
    }
}
