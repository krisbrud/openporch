//! Fault-injection sweeps over the durable boundary.
//!
//! `recovery.rs` stops a deployment at chosen points. These tests stop it at
//! every one: a run is repeated with its first commit and every later one
//! refused, then its second, and so on until a run ends before its crash
//! point, as when the process dies right before that write lands. Whatever
//! the in-memory run does after that never reaches the database, but what it
//! asks drivers to do is real, as it would be up to the moment of a crash.
//!
//! Branches run concurrently, so a run's commits interleave differently from
//! one repetition to the next; each repetition stops at its own `k`-th.
//!
//! After each crash the server restarts on the same database: the store is
//! opened again, interrupted deployments are failed, every uncertain resource
//! is recovered by observation, and the next deployment runs. The sweep checks
//! that no cloud object exists without a record of it, nothing was created or
//! deleted twice, identities recorded before the crash are kept, and the
//! environment converges.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

use super::*;

const RECOVER: &str = "/orgs/acme/apps/shop/envs/development/resources/recover";
const ENV: &str = "pr-9";
const DESCRIPTORS: [&str; 5] = [BASE, DB, DNS, ROUTE, WORKLOAD];

/// Refuses database commits from a chosen one on.
#[derive(Clone)]
struct Faults {
    commits: Arc<AtomicUsize>,
    fail_from: Arc<AtomicUsize>,
    refused: Arc<AtomicBool>,
}

impl Faults {
    /// Hooks every connection of the store's pool. The pool holds at most
    /// eight, so holding eight at once reaches all of them, and a hook stays
    /// with its connection when it goes back to the pool.
    async fn install(store: &SqliteStore) -> Self {
        let faults = Self {
            commits: Arc::default(),
            fail_from: Arc::new(AtomicUsize::new(usize::MAX)),
            refused: Arc::default(),
        };
        let mut connections = Vec::new();
        for _ in 0..8 {
            connections.push(store.pool().acquire().await.unwrap());
        }
        for connection in &mut connections {
            let f = faults.clone();
            connection
                .lock_handle()
                .await
                .unwrap()
                .set_commit_hook(move || {
                    let n = f.commits.fetch_add(1, Ordering::SeqCst);
                    let allowed = n < f.fail_from.load(Ordering::SeqCst);
                    if !allowed {
                        f.refused.store(true, Ordering::SeqCst);
                    }
                    allowed
                });
        }
        faults
    }

    /// Counts commits from zero, refusing the `k`-th and every later one.
    fn crash_at(&self, k: usize) {
        self.commits.store(0, Ordering::SeqCst);
        self.refused.store(false, Ordering::SeqCst);
        self.fail_from.store(k, Ordering::SeqCst);
    }
}

fn allowing(f: &Fixture, fake: &Arc<Fake>) -> Arc<Planner<SqliteStore, Fake>> {
    Arc::new(deleting(f, fake, &Arc::new(AllowAll::default())))
}

/// Runs `deployment` to its end or for at most ten seconds, then drops it, as
/// a dead process does.
async fn run_until_dead(f: &Fixture, fake: &Arc<Fake>, deployment: Deployment) {
    let planner = allowing(f, fake);
    let mut run = tokio::spawn(async move { planner.run(deployment).await });
    if tokio::time::timeout(Duration::from_secs(10), &mut run)
        .await
        .is_err()
    {
        run.abort();
        let _ = run.await;
    }
}

/// Restarts on the same database: the store is closed, which releases the
/// installation, and opened again; interrupted deployments are failed and
/// each uncertain resource is recovered by observation.
async fn restart(f: Fixture, fake: &Arc<Fake>, k: usize) -> Fixture {
    let Fixture {
        _dir: dir,
        store,
        env,
    } = f;
    store.pool().close().await;
    drop(Arc::into_inner(store).expect("only the fixture holds the store"));
    let ownership = Ownership::acquire(dir.path()).unwrap();
    let store = SqliteStore::open(ownership, ORG).await.unwrap();
    let f = Fixture {
        _dir: dir,
        store: Arc::new(store),
        env,
    };

    let report = f.store.recover_interrupted().await.unwrap();
    let planner = allowing(&f, fake);
    for op in report.uncertain {
        let recovered = planner
            .recover(ORG, "shop", f.env, &op.descriptor, audit("POST", RECOVER))
            .await
            .unwrap_or_else(|e| panic!("commit {k}: recovering {}: {e}", op.descriptor));
        assert!(
            matches!(
                recovered,
                Recovered::Provisioned | Recovered::Deleted | Recovered::Repeatable(_)
            ),
            "commit {k}: {} recovered as {recovered:?}",
            op.descriptor
        );
    }
    f
}

/// Each resource's GUResID.
async fn identities(f: &Fixture) -> BTreeMap<String, String> {
    f.store
        .active_resources(ORG, "shop", f.env)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.descriptor, r.guresid))
        .collect()
}

/// Every cloud object belongs to a resource the database tracks under that
/// identity, so nothing exists that a teardown would not find.
async fn nothing_untracked(f: &Fixture, fake: &Fake, k: usize) {
    let tracked: BTreeMap<String, String> = identities(f)
        .await
        .into_iter()
        .map(|(descriptor, guresid)| (guresid, descriptor))
        .collect();
    let objects = fake.objects.lock().unwrap().clone();
    for (guresid, descriptor) in objects {
        assert_eq!(
            tracked.get(&guresid),
            Some(&descriptor),
            "commit {k}: the {descriptor} object {guresid} is not tracked"
        );
    }
}

/// Deploys `web` in a new environment, with every commit allowed.
async fn deployed(fake: &Arc<Fake>) -> Fixture {
    let f = Fixture::in_env(ENV).await;
    let outcomes = allowing(&f, fake).run(f.accept().await).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_at_any_commit_of_a_deployment_is_recovered() {
    let mut k = 0;
    loop {
        let f = Fixture::in_env(ENV).await;
        let fake = Fake::with(&[]);
        let faults = Faults::install(&f.store).await;
        let deployment = f.accept().await;
        faults.crash_at(k);
        run_until_dead(&f, &fake, deployment).await;
        // Once the run makes fewer than `k` commits, it ran to its end, and
        // the checks below see an ordinary deployment.
        let crashed = faults.refused.load(Ordering::SeqCst);

        let f = restart(f, &fake, k).await;
        nothing_untracked(&f, &fake, k).await;
        let before = identities(&f).await;

        // The next deployment converges under the identities recorded before
        // the crash, having created each object once.
        let outcomes = allowing(&f, &fake).run(f.accept().await).await;
        assert!(
            outcomes.values().all(|o| *o == Outcome::Provisioned),
            "commit {k}: {outcomes:?}"
        );
        let after = identities(&f).await;
        for (descriptor, guresid) in &before {
            assert_eq!(
                after.get(descriptor),
                Some(guresid),
                "commit {k}: {descriptor}"
            );
        }
        for descriptor in DESCRIPTORS {
            assert_eq!(fake.creations(descriptor), 1, "commit {k}: {descriptor}");
        }
        nothing_untracked(&f, &fake, k).await;
        assert_eq!(
            fake.objects.lock().unwrap().len(),
            DESCRIPTORS.len(),
            "commit {k}"
        );
        if !crashed {
            break;
        }
        k += 1;
    }
    assert!(k >= 10, "only {k} commits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_at_any_commit_of_a_teardown_is_recovered() {
    let mut k = 0;
    loop {
        let fake = Fake::with(&[]);
        let f = deployed(&fake).await;
        let owned: BTreeSet<String> = identities(&f).await.into_values().collect();
        let faults = Faults::install(&f.store).await;
        let teardown = f.accept_teardown().await.unwrap();
        faults.crash_at(k);
        run_until_dead(&f, &fake, teardown).await;
        // Once the run makes fewer than `k` commits, it ran to its end, and
        // the checks below see an ordinary deployment.
        let crashed = faults.refused.load(Ordering::SeqCst);

        // What still exists is still tracked, so the retry can delete it.
        let f = restart(f, &fake, k).await;
        nothing_untracked(&f, &fake, k).await;
        if f.env_state().await.is_some() {
            let teardown = f.accept_teardown().await.unwrap();
            let outcomes = allowing(&f, &fake).run(teardown).await;
            assert!(
                outcomes.values().all(|o| *o == Outcome::Deleted),
                "commit {k}: {outcomes:?}"
            );
        }
        assert_eq!(f.env_state().await, None, "commit {k}");
        assert!(
            fake.objects.lock().unwrap().is_empty(),
            "commit {k}: {:?}",
            fake.objects.lock().unwrap()
        );

        // Each owned object was deleted once, and nothing else was.
        let deleted: Vec<String> = fake
            .deleted
            .lock()
            .unwrap()
            .iter()
            .map(|j| j.job.guresid.to_string())
            .collect();
        let unique: BTreeSet<String> = deleted.iter().cloned().collect();
        assert_eq!(unique.len(), deleted.len(), "commit {k}: {deleted:?}");
        assert_eq!(unique, owned, "commit {k}");
        if !crashed {
            break;
        }
        k += 1;
    }
    assert!(k >= 10, "only {k} commits");
}
