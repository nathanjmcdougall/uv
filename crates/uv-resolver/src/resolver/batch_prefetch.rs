use std::cmp::min;

use itertools::Itertools;
use pubgrub::{Id, Range, State, Term};
use rustc_hash::FxHashMap;
use tokio::sync::mpsc::Sender;
use tracing::{debug, trace};

use crate::candidate_selector::CandidateSelector;
use crate::dependency_provider::UvDependencyProvider;
use crate::pubgrub::{PubGrubPackage, PubGrubPackageInner};
use crate::resolver::Request;
use crate::{
    InMemoryIndex, PythonRequirement, ResolveError, ResolverEnvironment, VersionsResponse,
};
use uv_distribution_types::{CompatibleDist, DistributionMetadata, IndexCapabilities, IndexUrl};
use uv_pep440::Version;

enum BatchPrefetchStrategy {
    /// Go through the next versions assuming the existing selection and its constraints
    /// remain.
    Compatible {
        compatible: Range<Version>,
        previous: Version,
    },
    /// We encounter cases (botocore) where the above doesn't work: Say we previously selected
    /// a==x.y.z, which depends on b==x.y.z. a==x.y.z is incompatible, but we don't know that
    /// yet. We just selected b==x.y.z and want to prefetch, since for all versions of a we try,
    /// we have to wait for the matching version of b. The exiting range gives us only one version
    /// of b, so the compatible strategy doesn't prefetch any version. Instead, we try the next
    /// heuristic where the next version of b will be x.y.(z-1) and so forth.
    InOrder { previous: Version },
}

/// Prefetch a large number of versions if we already unsuccessfully tried many versions.
///
/// This is an optimization specifically targeted at cold cache urllib3/boto3/botocore, where we
/// have to fetch the metadata for a lot of versions.
///
/// Note that these all heuristics that could totally prefetch lots of irrelevant versions.
#[derive(Default)]
pub(crate) struct BatchPrefetcher {
    tried_versions: FxHashMap<Id<PubGrubPackage>, usize>,
    last_prefetch: FxHashMap<Id<PubGrubPackage>, usize>,
}

impl BatchPrefetcher {
    /// Prefetch a large number of versions if we already unsuccessfully tried many versions.
    pub(crate) fn prefetch_batches(
        &mut self,
        id: Id<PubGrubPackage>,
        next: &PubGrubPackage,
        index: Option<&IndexUrl>,
        version: &Version,
        current_range: &Range<Version>,
        unchangeable_constraints: Option<&Term<Range<Version>>>,
        python_requirement: &PythonRequirement,
        request_sink: &Sender<Request>,
        in_memory: &InMemoryIndex,
        capabilities: &IndexCapabilities,
        selector: &CandidateSelector,
        env: &ResolverEnvironment,
    ) -> anyhow::Result<(), ResolveError> {
        let PubGrubPackageInner::Package {
            name,
            extra: None,
            dev: None,
            marker: None,
        } = &**next
        else {
            return Ok(());
        };

        let (num_tried, do_prefetch) = self.should_prefetch(id);
        if !do_prefetch {
            return Ok(());
        }
        let total_prefetch = min(num_tried, 50);

        // This is immediate, we already fetched the version map.
        let versions_response = if let Some(index) = index {
            in_memory
                .explicit()
                .wait_blocking(&(name.clone(), index.clone()))
                .ok_or_else(|| ResolveError::UnregisteredTask(name.to_string()))?
        } else {
            in_memory
                .implicit()
                .wait_blocking(name)
                .ok_or_else(|| ResolveError::UnregisteredTask(name.to_string()))?
        };

        let VersionsResponse::Found(ref version_map) = *versions_response else {
            return Ok(());
        };

        let mut phase = BatchPrefetchStrategy::Compatible {
            compatible: current_range.clone(),
            previous: version.clone(),
        };
        let mut prefetch_count = 0;
        for _ in 0..total_prefetch {
            let candidate = match phase {
                BatchPrefetchStrategy::Compatible {
                    compatible,
                    previous,
                } => {
                    if let Some(candidate) =
                        selector.select_no_preference(name, &compatible, version_map, env)
                    {
                        let compatible = compatible.intersection(
                            &Range::singleton(candidate.version().clone()).complement(),
                        );
                        phase = BatchPrefetchStrategy::Compatible {
                            compatible,
                            previous: candidate.version().clone(),
                        };
                        candidate
                    } else {
                        // We exhausted the compatible version, switch to ignoring the existing
                        // constraints on the package and instead going through versions in order.
                        phase = BatchPrefetchStrategy::InOrder { previous };
                        continue;
                    }
                }
                BatchPrefetchStrategy::InOrder { previous } => {
                    let mut range = if selector.use_highest_version(name, env) {
                        Range::strictly_lower_than(previous)
                    } else {
                        Range::strictly_higher_than(previous)
                    };
                    // If we have constraints from root, don't go beyond those. Example: We are
                    // prefetching for foo 1.60 and have a dependency for `foo>=1.50`, so we should
                    // only prefetch 1.60 to 1.50, knowing 1.49 will always be rejected.
                    if let Some(unchangeable_constraints) = unchangeable_constraints {
                        range = match unchangeable_constraints {
                            Term::Positive(constraints) => range.intersection(constraints),
                            Term::Negative(negative_constraints) => {
                                range.intersection(&negative_constraints.complement())
                            }
                        };
                    }
                    if let Some(candidate) =
                        selector.select_no_preference(name, &range, version_map, env)
                    {
                        phase = BatchPrefetchStrategy::InOrder {
                            previous: candidate.version().clone(),
                        };
                        candidate
                    } else {
                        // Both strategies exhausted their candidates.
                        break;
                    }
                }
            };

            let Some(dist) = candidate.compatible() else {
                continue;
            };

            // Avoid prefetching source distributions, which could be expensive.
            let Some(wheel) = dist.wheel() else {
                continue;
            };

            // Avoid prefetching built distributions that don't support _either_ PEP 658 (`.metadata`)
            // or range requests.
            if !(wheel.file.dist_info_metadata
                || capabilities.supports_range_requests(&wheel.index))
            {
                debug!("Abandoning prefetch for {wheel} due to missing registry capabilities");
                return Ok(());
            }

            // Avoid prefetching for distributions that don't satisfy the Python requirement.
            match dist {
                CompatibleDist::InstalledDist(_) => {}
                CompatibleDist::SourceDist { sdist, .. }
                | CompatibleDist::IncompatibleWheel { sdist, .. } => {
                    // Source distributions must meet both the _target_ Python version and the
                    // _installed_ Python version (to build successfully).
                    if let Some(requires_python) = sdist.file.requires_python.as_ref() {
                        if !python_requirement
                            .installed()
                            .is_contained_by(requires_python)
                        {
                            continue;
                        }
                        if !python_requirement.target().is_contained_by(requires_python) {
                            continue;
                        }
                    }
                }
                CompatibleDist::CompatibleWheel { wheel, .. } => {
                    // Wheels must meet the _target_ Python version.
                    if let Some(requires_python) = wheel.file.requires_python.as_ref() {
                        if !python_requirement.target().is_contained_by(requires_python) {
                            continue;
                        }
                    }
                }
            };

            let dist = dist.for_resolution();

            // Emit a request to fetch the metadata for this version.
            trace!(
                "Prefetching {prefetch_count} ({}) {}",
                match phase {
                    BatchPrefetchStrategy::Compatible { .. } => "compatible",
                    BatchPrefetchStrategy::InOrder { .. } => "in order",
                },
                dist
            );
            prefetch_count += 1;

            if in_memory.distributions().register(candidate.version_id()) {
                let request = Request::from(dist);
                request_sink.blocking_send(request)?;
            }
        }

        debug!("Prefetching {prefetch_count} {name} versions");

        self.last_prefetch.insert(id, num_tried);
        Ok(())
    }

    /// Each time we tried a version for a package, we register that here.
    pub(crate) fn version_tried(&mut self, id: Id<PubGrubPackage>, package: &PubGrubPackage) {
        // Only track base packages, no virtual packages from extras.
        if matches!(
            &**package,
            PubGrubPackageInner::Package {
                extra: None,
                dev: None,
                marker: None,
                ..
            }
        ) {
            *self.tried_versions.entry(id).or_default() += 1;
        }
    }

    /// After 5, 10, 20, 40 tried versions, prefetch that many versions to start early but not
    /// too aggressive. Later we schedule the prefetch of 50 versions every 20 versions, this gives
    /// us a good buffer until we see prefetch again and is high enough to saturate the task pool.
    fn should_prefetch(&self, id: Id<PubGrubPackage>) -> (usize, bool) {
        let num_tried = self.tried_versions.get(&id).copied().unwrap_or_default();
        let previous_prefetch = self.last_prefetch.get(&id).copied().unwrap_or_default();
        let do_prefetch = (num_tried >= 5 && previous_prefetch < 5)
            || (num_tried >= 10 && previous_prefetch < 10)
            || (num_tried >= 20 && previous_prefetch < 20)
            || (num_tried >= 20 && num_tried - previous_prefetch >= 20);
        (num_tried, do_prefetch)
    }

    /// Log stats about how many versions we tried.
    ///
    /// Note that they may be inflated when we count the same version repeatedly during
    /// backtracking.
    pub(crate) fn log_tried_versions(&self, state: &State<UvDependencyProvider>) {
        let total_versions: usize = self.tried_versions.values().sum();
        let mut tried_versions: Vec<_> = self
            .tried_versions
            .iter()
            .map(|(id, count)| (&state.package_store[*id], *count))
            .collect();
        tried_versions.sort_by(|(p1, c1), (p2, c2)| {
            c1.cmp(c2)
                .reverse()
                .then(p1.to_string().cmp(&p2.to_string()))
        });
        let counts = tried_versions
            .iter()
            .map(|(package, count)| format!("{package} {count}"))
            .join(", ");
        debug!("Tried {total_versions} versions: {counts}");
    }
}
