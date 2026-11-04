use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[test]
fn repeated_grant_request_revoke_and_disconnect_releases_state_and_budgets() {
    let fixture = Fixture::new();
    let budgets = crate::budget::Budgets::with_clock(fixture.clock.clone()).unwrap();
    for second in 1..=100 {
        fixture.clock.set(second);
        let route = fixture.authority.register_route().unwrap();
        let budget = budgets.open_route(route.id()).unwrap();
        let grant = fixture
            .authority
            .prepare(&route, vec![fixture.target], Permission::Read, 60, None)
            .unwrap()
            .authenticated()
            .unwrap()
            .grant;
        let request = Request {
            grant,
            target: fixture.target,
            operation: Operation::Read,
            sequence: 1,
        };
        let permit = fixture.endpoint.accept(&route, request).unwrap();
        let reservation = budget.acquire(4096).unwrap();
        permit.execute(fixture.target, || ()).unwrap();
        fixture.authority.revoke(grant).unwrap();
        assert!(permit.execute(fixture.target, || ()).is_err());
        drop((permit, reservation, budget, route));
        assert_eq!(budgets.retained().unwrap(), (0, 0));
        let state = fixture.authority.shared.state.lock().unwrap();
        assert!(state.grants.is_empty());
        assert!(state.pending.is_none());
        assert_eq!(state.routes.len(), 1);
    }
}

#[test]
fn closing_the_authority_invalidates_work_and_cannot_be_reopened() {
    let fixture = Fixture::new();
    let grant = fixture.grant();
    let permit = fixture
        .endpoint
        .accept(&fixture.route, fixture.request(grant, 1))
        .unwrap();
    let pending = fixture.pending(Some(grant));
    fixture.authority.close();
    assert!(permit.execute(fixture.target, || ()).is_err());
    assert!(pending.authenticated().is_err());
    assert!(fixture.endpoint.list(&fixture.route).is_err());
    assert!(fixture.authority.register_route().is_err());
    assert!(fixture.authority.start_monitor().is_err());
    fixture.authority.close();
}

#[test]
fn missing_native_monitor_heartbeat_revokes_at_the_execution_boundary() {
    let fixture = Fixture::new();
    let monitor = fixture.authority.start_monitor().unwrap();
    let grant = fixture.grant();
    let permit = fixture
        .endpoint
        .accept(&fixture.route, fixture.request(grant, 1))
        .unwrap();
    fixture.clock.set(5);
    let called = AtomicBool::new(false);
    assert!(permit
        .execute(fixture.target, || called.store(true, Ordering::Relaxed))
        .is_err());
    assert!(!called.load(Ordering::Relaxed));
    assert!(monitor.heartbeat().is_err());
    assert!(fixture.endpoint.list(&fixture.route).is_err());
}

#[test]
fn old_monitor_cannot_revoke_or_extend_a_replacement_session() {
    let fixture = Fixture::new();
    let old = fixture.authority.start_monitor().unwrap();
    let current = fixture.authority.start_monitor().unwrap();
    let grant = fixture.grant();
    assert!(old.heartbeat().is_err());
    drop(old);
    let permit = fixture
        .endpoint
        .accept(&fixture.route, fixture.request(grant, 1))
        .unwrap();
    assert!(permit.execute(fixture.target, || ()).is_ok());
    current.close();
    assert!(permit.execute(fixture.target, || ()).is_err());
}

#[test]
fn cancelling_native_work_also_revokes_a_result_not_yet_delivered_to_the_ui() {
    let fixture = Fixture::new();
    let approval = fixture.pending(None);
    let cancellation = approval.cancellation();
    let grant = approval.authenticated().unwrap().grant;
    cancellation.cancel();
    assert!(fixture
        .endpoint
        .accept(&fixture.route, fixture.request(grant, 1))
        .is_err());
    let pending = fixture.pending(None);
    let cancellation = pending.cancellation();
    cancellation.cancel();
    assert!(pending.authenticated().is_err());
}

#[derive(Default)]
struct TestClock {
    seconds: AtomicU64,
    failed: AtomicBool,
}

impl Clock for TestClock {
    fn now(&self) -> Result<Tick> {
        if self.failed.load(Ordering::Relaxed) {
            return Err(Error::Clock);
        }
        Ok(Tick::seconds(self.seconds.load(Ordering::Relaxed)))
    }
}

impl TestClock {
    fn set(&self, seconds: u64) {
        self.seconds.store(seconds, Ordering::Relaxed);
    }
}

struct Fixture {
    authority: Authority,
    endpoint: Endpoint,
    clock: Arc<TestClock>,
    route: Route,
    target: Target,
}

#[test]
fn all_local_membership_changes_preserve_expiry_and_reject_old_work() {
    let fixture = Fixture::new();
    let approval = fixture
        .authority
        .prepare_all_local(&fixture.route, Permission::ReadAndInput, 60, None)
        .unwrap();
    assert_eq!(
        approval.details().unwrap().scope,
        GrantScope::AllLocalTerminals
    );
    let info = approval.authenticated().unwrap();
    assert!(info.targets.is_empty());
    let grant = info.grant;
    assert!(fixture
        .endpoint
        .accept(&fixture.route, fixture.request(grant, 1))
        .is_err());
    fixture
        .authority
        .replace_all_local_targets(&fixture.route, grant, vec![fixture.target])
        .unwrap();
    let permit = fixture
        .endpoint
        .accept(&fixture.route, fixture.request(grant, 2))
        .unwrap();
    assert!(permit.execute(fixture.target, || ()).is_ok());
    fixture.clock.set(10);
    fixture.authority.invalidate_target(fixture.target).unwrap();
    let remaining = fixture.endpoint.list(&fixture.route).unwrap();
    assert_eq!(remaining.len(), 1);
    assert!(remaining[0].targets.is_empty());
    assert_eq!(remaining[0].remaining, Duration::from_secs(50));
    assert!(permit.execute(fixture.target, || ()).is_err());

    let replacement = Target::new(fixture.target.pane, fixture.target.owner).unwrap();
    fixture
        .authority
        .replace_all_local_targets(&fixture.route, grant, vec![replacement])
        .unwrap();
    let request = Request {
        target: replacement,
        ..fixture.request(grant, 3)
    };
    let permit = fixture.endpoint.accept(&fixture.route, request).unwrap();
    assert!(permit.execute(replacement, || ()).is_ok());
    assert!(fixture.endpoint.accept(&fixture.route, request).is_err());
    assert!(fixture
        .endpoint
        .accept(&fixture.route, fixture.request(grant, 4))
        .is_err());
    fixture.clock.set(60);
    assert!(fixture
        .authority
        .replace_all_local_targets(&fixture.route, grant, vec![replacement])
        .is_err());
    assert!(permit.execute(replacement, || ()).is_err());
}

#[test]
fn all_local_refresh_cannot_widen_selected_grants_or_cross_routes() {
    let fixture = Fixture::new();
    let selected = fixture.grant();
    assert!(fixture
        .authority
        .replace_all_local_targets(&fixture.route, selected, vec![])
        .is_err());
    assert!(fixture
        .authority
        .prepare_all_local(&fixture.route, Permission::Read, 60, Some(selected))
        .is_err());
    fixture.authority.revoke(selected).unwrap();
    let grant = fixture
        .authority
        .prepare_all_local(&fixture.route, Permission::Read, 60, None)
        .unwrap()
        .authenticated()
        .unwrap()
        .grant;
    let other = fixture.authority.register_route().unwrap();
    assert!(fixture
        .authority
        .replace_all_local_targets(&other, grant, vec![fixture.target])
        .is_err());
    assert!(fixture
        .authority
        .prepare(
            &fixture.route,
            vec![fixture.target],
            Permission::Read,
            60,
            Some(grant)
        )
        .is_err());
    assert!(fixture
        .authority
        .prepare(
            &fixture.route,
            vec![fixture.target],
            Permission::Read,
            60,
            None
        )
        .is_err());
    assert!(fixture
        .authority
        .replace_all_local_targets(&fixture.route, grant, vec![fixture.target; 2])
        .is_err());
    let oversized = (0..=MAX_TARGETS)
        .map(|pane| Target {
            pane: pane as u64,
            ..fixture.target
        })
        .collect();
    assert_eq!(
        fixture
            .authority
            .replace_all_local_targets(&fixture.route, grant, oversized),
        Err(Error::TooLarge)
    );
    fixture
        .authority
        .revoke_current(&fixture.route, grant.id)
        .unwrap();
    assert!(fixture
        .authority
        .replace_all_local_targets(&fixture.route, grant, vec![fixture.target])
        .is_err());
}

#[test]
fn all_local_renewal_retains_scope_and_invalidates_previous_membership() {
    let fixture = Fixture::new();
    let grant = fixture
        .authority
        .prepare_all_local(&fixture.route, Permission::Read, 60, None)
        .unwrap()
        .authenticated()
        .unwrap()
        .grant;
    fixture
        .authority
        .replace_all_local_targets(&fixture.route, grant, vec![fixture.target])
        .unwrap();
    let renewal = fixture
        .authority
        .prepare_all_local(&fixture.route, Permission::Read, 60, Some(grant))
        .unwrap()
        .authenticated()
        .unwrap();
    assert_eq!(renewal.scope, GrantScope::AllLocalTerminals);
    assert!(renewal.targets.is_empty());
    assert!(fixture
        .authority
        .replace_all_local_targets(&fixture.route, grant, vec![fixture.target])
        .is_err());
    fixture
        .authority
        .replace_all_local_targets(&fixture.route, renewal.grant, vec![fixture.target])
        .unwrap();
    assert!(fixture
        .endpoint
        .accept(
            &fixture.route,
            Request {
                operation: Operation::Read,
                ..fixture.request(renewal.grant, 1)
            }
        )
        .is_ok());
    assert!(fixture
        .endpoint
        .accept(&fixture.route, fixture.request(renewal.grant, 2))
        .is_err());
}

impl Fixture {
    fn new() -> Self {
        let clock = Arc::new(TestClock::default());
        let (authority, endpoint) = Authority::with_clock(clock.clone());
        authority.session_changed(true).unwrap();
        let route = authority.register_route().unwrap();
        let target = Target::new(1, Id::from_bytes([1; 16])).unwrap();
        Self {
            authority,
            endpoint,
            clock,
            route,
            target,
        }
    }

    fn pending(&self, renewal: Option<GrantRef>) -> Approval {
        self.authority
            .prepare(
                &self.route,
                vec![self.target],
                Permission::ReadAndInput,
                60,
                renewal,
            )
            .unwrap()
    }

    fn grant(&self) -> GrantRef {
        self.pending(None).authenticated().unwrap().grant
    }

    fn request(&self, grant: GrantRef, sequence: u64) -> Request {
        Request {
            grant,
            target: self.target,
            operation: Operation::Input,
            sequence,
        }
    }
}

#[test]
fn rejected_or_abandoned_approval_creates_no_grant() {
    let f = Fixture::new();
    f.pending(None).reject();
    assert!(f.endpoint.list(&f.route).unwrap().is_empty());
    drop(f.pending(None));
    assert!(f.endpoint.list(&f.route).unwrap().is_empty());
    assert!(f.pending(None).authenticated().is_ok());
}

#[test]
fn failed_renewal_keeps_the_original_scope_and_deadline() {
    let f = Fixture::new();
    let grant = f.grant();
    f.clock.set(30);
    f.pending(Some(grant)).reject();
    let grants = f.endpoint.list(&f.route).unwrap();
    assert_eq!(grants[0].grant, grant);
    assert_eq!(grants[0].remaining, Duration::from_secs(30));
    f.clock.set(60);
    assert!(f.endpoint.list(&f.route).unwrap().is_empty());
}

#[test]
fn expired_or_revoked_grant_cannot_be_revived_by_a_late_renewal() {
    for revoke in [false, true] {
        let f = Fixture::new();
        let grant = f.grant();
        let pending = f.pending(Some(grant));
        if revoke {
            f.authority.revoke(grant).unwrap();
        } else {
            f.clock.set(60);
        }
        assert!(matches!(pending.authenticated(), Err(Error::StaleApproval)));
        assert!(f.endpoint.list(&f.route).unwrap().is_empty());
    }
}

#[test]
fn explicit_revocation_covers_a_renewal_completed_after_the_ui_snapshot() {
    for operation in [Operation::Read, Operation::Input] {
        let f = Fixture::new();
        let displayed = f.grant();
        let renewed = f.pending(Some(displayed)).authenticated().unwrap().grant;
        let mut request = f.request(renewed, 1);
        request.operation = operation;
        let permit = f.endpoint.accept(&f.route, request).unwrap();
        permit.execute(f.target, || ()).unwrap();

        f.authority.revoke_current(&f.route, displayed.id).unwrap();

        let mut submitted = false;
        assert!(permit.execute(f.target, || submitted = true).is_err());
        assert!(!submitted);
        assert!(f.endpoint.list(&f.route).unwrap().is_empty());
        assert!(f.endpoint.accept(&f.route, f.request(renewed, 2)).is_err());
        f.authority.revoke_current(&f.route, displayed.id).unwrap();
    }
}

#[test]
fn explicit_revocation_cancels_pending_renewals_of_any_version() {
    for renew_first in [false, true] {
        let f = Fixture::new();
        let displayed = f.grant();
        let current = if renew_first {
            f.pending(Some(displayed)).authenticated().unwrap().grant
        } else {
            displayed
        };
        let pending = f.pending(Some(current));
        f.authority.revoke_current(&f.route, displayed.id).unwrap();
        assert!(matches!(pending.authenticated(), Err(Error::StaleApproval)));
        assert!(f.endpoint.list(&f.route).unwrap().is_empty());
        assert!(f.pending(None).authenticated().is_ok());
    }
}

#[test]
fn explicit_revocation_leaves_other_grants_and_approvals_intact() {
    for renewal in [false, true] {
        let f = Fixture::new();
        let displayed = f.grant();
        let other_target = Target::new(2, f.target.owner).unwrap();
        let other = if renewal {
            Some(
                f.authority
                    .prepare(&f.route, vec![other_target], Permission::Read, 60, None)
                    .unwrap()
                    .authenticated()
                    .unwrap()
                    .grant,
            )
        } else {
            None
        };
        let pending = f
            .authority
            .prepare(&f.route, vec![other_target], Permission::Read, 60, other)
            .unwrap();
        f.authority.revoke_current(&f.route, displayed.id).unwrap();
        let approved = pending.authenticated().unwrap();
        let grants = f.endpoint.list(&f.route).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].grant, approved.grant);
        assert_eq!(grants[0].targets, [other_target]);
    }
}

#[test]
fn explicit_revocation_cannot_cross_route_or_authority_boundaries() {
    let f = Fixture::new();
    let current = f.grant();
    let pending = f.pending(Some(current));
    let other_route = f.authority.register_route().unwrap();
    f.authority
        .revoke_current(&other_route, current.id)
        .unwrap();
    let other_authority = Fixture::new();
    assert!(f
        .authority
        .revoke_current(&other_authority.route, current.id)
        .is_err());
    let permit = f.endpoint.accept(&f.route, f.request(current, 1)).unwrap();
    assert!(permit.execute(f.target, || ()).is_ok());
    assert!(pending.authenticated().is_ok());
}

#[test]
fn version_specific_cleanup_preserves_a_replacement_and_its_pending_renewal() {
    let f = Fixture::new();
    let old = f.grant();
    let current = f.pending(Some(old)).authenticated().unwrap().grant;
    let pending = f.pending(Some(current));
    f.authority.revoke(old).unwrap();
    let permit = f.endpoint.accept(&f.route, f.request(current, 1)).unwrap();
    assert!(permit.execute(f.target, || ()).is_ok());
    assert!(pending.authenticated().is_ok());
}

#[test]
fn lock_and_unlock_invalidate_the_old_authentication_callback() {
    let f = Fixture::new();
    let pending = f.pending(None);
    f.authority.session_changed(false).unwrap();
    f.authority.session_changed(true).unwrap();
    assert!(matches!(pending.authenticated(), Err(Error::StaleApproval)));
    assert!(f.endpoint.list(&f.route).unwrap().is_empty());
}

#[test]
fn authentication_has_an_absolute_deadline() {
    let f = Fixture::new();
    let pending = f.pending(None);
    f.clock.set(AUTH_TIMEOUT_SECS);
    assert!(matches!(pending.authenticated(), Err(Error::StaleApproval)));
}

#[test]
fn input_is_checked_again_at_each_submission_step() {
    let f = Fixture::new();
    let grant = f.grant();
    let permit = f.endpoint.accept(&f.route, f.request(grant, 1)).unwrap();
    let mut submissions = vec![];
    permit
        .execute(f.target, || submissions.push("text"))
        .unwrap();
    f.authority.revoke(grant).unwrap();
    assert!(permit
        .execute(f.target, || submissions.push("enter"))
        .is_err());
    assert_eq!(submissions, ["text"]);
}

#[test]
fn snapshot_revoked_before_send_is_not_released() {
    let f = Fixture::new();
    let grant = f.grant();
    let mut request = f.request(grant, 1);
    request.operation = Operation::Read;
    let permit = f.endpoint.accept(&f.route, request).unwrap();
    let body = permit.execute(f.target, || "example output").unwrap();
    f.authority.invalidate_target(f.target).unwrap();
    let mut released = false;
    assert!(permit
        .execute(f.target, || {
            let _ = body;
            released = true;
        })
        .is_err());
    assert!(!released);
}

#[test]
fn renewal_invalidates_queued_work_without_resetting_the_replay_watermark() {
    let f = Fixture::new();
    let old = f.grant();
    let permit = f.endpoint.accept(&f.route, f.request(old, 7)).unwrap();
    let new = f.pending(Some(old)).authenticated().unwrap().grant;
    assert_eq!(new.id, old.id);
    assert_eq!(new.version, old.version + 1);
    assert!(permit.execute(f.target, || ()).is_err());
    assert!(matches!(
        f.endpoint.accept(&f.route, f.request(new, 7)),
        Err(Error::Replay)
    ));
    assert!(f.endpoint.accept(&f.route, f.request(new, 8)).is_ok());
}

#[test]
fn dropping_a_request_does_not_make_input_retryable() {
    let f = Fixture::new();
    let grant = f.grant();
    drop(f.endpoint.accept(&f.route, f.request(grant, 42)).unwrap());
    assert!(matches!(
        f.endpoint.accept(&f.route, f.request(grant, 42)),
        Err(Error::Replay)
    ));
}

#[test]
fn route_shutdown_cancels_pending_approval_and_queued_requests() {
    let f = Fixture::new();
    let grant = f.grant();
    let permit = f.endpoint.accept(&f.route, f.request(grant, 1)).unwrap();
    let pending = f.pending(Some(grant));
    drop(f.route);
    assert!(permit.execute(f.target, || ()).is_err());
    assert!(matches!(pending.authenticated(), Err(Error::StaleApproval)));
}

#[test]
fn an_identical_pane_number_on_another_route_or_incarnation_is_denied() {
    let f = Fixture::new();
    let grant = f.grant();
    let other = f.authority.register_route().unwrap();
    assert!(f.endpoint.accept(&other, f.request(grant, 1)).is_err());
    let permit = f.endpoint.accept(&f.route, f.request(grant, 1)).unwrap();
    let new = Target::new(f.target.pane, f.target.owner).unwrap();
    assert!(permit.execute(new, || ()).is_err());
    let new_owner = Target {
        owner: Id::from_bytes([2; 16]),
        ..f.target
    };
    assert!(permit.execute(new_owner, || ()).is_err());
}

#[test]
fn unavailable_clock_or_backwards_time_revokes_existing_authorization() {
    for backwards in [false, true] {
        let f = Fixture::new();
        f.clock.set(10);
        let grant = f.grant();
        let permit = f.endpoint.accept(&f.route, f.request(grant, 1)).unwrap();
        if backwards {
            f.clock.set(9);
        } else {
            f.clock.failed.store(true, Ordering::Relaxed);
        }
        assert!(matches!(permit.execute(f.target, || ()), Err(Error::Clock)));
        f.clock.failed.store(false, Ordering::Relaxed);
        f.clock.set(11);
        f.authority.session_changed(true).unwrap();
        assert!(f.endpoint.list(&f.route).unwrap().is_empty());
    }
}

#[test]
fn a_pending_request_cannot_extend_its_deadline_by_making_progress() {
    let f = Fixture::new();
    let grant = f.grant();
    let permit = f.endpoint.accept(&f.route, f.request(grant, 1)).unwrap();
    for second in 0..REQUEST_TIMEOUT_SECS {
        f.clock.set(second);
        permit.execute(f.target, || ()).unwrap();
    }
    f.clock.set(REQUEST_TIMEOUT_SECS);
    assert!(matches!(
        permit.execute(f.target, || ()),
        Err(Error::Timeout)
    ));
}

#[test]
fn read_only_grants_cannot_be_used_for_input() {
    let f = Fixture::new();
    let grant = f
        .authority
        .prepare(&f.route, vec![f.target], Permission::Read, 60, None)
        .unwrap()
        .authenticated()
        .unwrap()
        .grant;
    assert!(f.endpoint.accept(&f.route, f.request(grant, 1)).is_err());
    let mut request = f.request(grant, 1);
    request.operation = Operation::Read;
    assert!(f.endpoint.accept(&f.route, request).is_ok());
}

#[test]
fn a_route_has_one_round_trippable_export_per_pane() {
    let f = Fixture::new();
    let grant = f.grant();
    let captured_again = Target::new(f.target.pane, Id::from_bytes([9; 16])).unwrap();
    assert!(matches!(
        f.authority
            .prepare(&f.route, vec![captured_again], Permission::Read, 60, None),
        Err(Error::Busy)
    ));
    assert!(f
        .authority
        .prepare(
            &f.route,
            vec![captured_again],
            Permission::Read,
            60,
            Some(grant)
        )
        .is_ok());
}

#[test]
fn forwarding_may_shorten_but_cannot_extend_executor_deadlines() {
    let f = Fixture::new();
    let grant = f.grant();
    assert!(f
        .endpoint
        .accept_for(&f.route, f.request(grant, 1), Duration::from_secs(11))
        .is_err());
    let permit = f
        .endpoint
        .accept_for(&f.route, f.request(grant, 1), Duration::from_secs(2))
        .unwrap();
    let list = f
        .endpoint
        .accept_list_for(&f.route, 2, Duration::from_secs(1))
        .unwrap();
    f.clock.set(1);
    assert!(list.execute(&[(grant, f.target)], || ()).is_err());
    assert!(permit.execute(f.target, || ()).is_ok());
    f.clock.set(2);
    assert_eq!(permit.execute(f.target, || ()), Err(Error::Timeout));
}

#[test]
fn repeated_authorize_request_revoke_disconnect_cycles_release_all_permits() {
    use crate::budget::Budgets;
    use crate::wire::Assembly;
    let clock = Arc::new(TestClock::default());
    let (authority, endpoint) = Authority::with_clock(clock.clone());
    let budgets = Budgets::with_clock(clock.clone()).unwrap();
    authority.session_changed(true).unwrap();
    for cycle in 0..100 {
        clock.set(cycle * 20);
        let route = authority.register_route().unwrap();
        let budget = budgets.open_route(route.id()).unwrap();
        let target = Target::new(cycle, Id::from_bytes([1; 16])).unwrap();
        let grant = authority
            .prepare(&route, vec![target], Permission::ReadAndInput, 60, None)
            .unwrap()
            .authenticated()
            .unwrap()
            .grant;
        let request = Request {
            grant,
            target,
            operation: Operation::Input,
            sequence: 1,
        };
        let permit = endpoint.accept(&route, request).unwrap();
        let mut body = Assembly::input(7, &budget).unwrap();
        body.append(0, b"example").unwrap();
        let body = body.finish().unwrap();
        assert_eq!(permit.execute(target, || body.text().len()).unwrap(), 7);
        authority.revoke(grant).unwrap();
        assert!(permit.execute(target, || ()).is_err());
        assert!(endpoint.accept(&route, request).is_err());
        drop((body, permit, route, budget));
        assert_eq!(budgets.retained().unwrap(), (0, 0));
        let state = authority.shared.state.lock().unwrap();
        assert!(state.grants.is_empty());
        assert!(state.routes.is_empty());
        assert!(state.pending.is_none());
    }
}

#[test]
fn authority_exit_revokes_even_when_the_data_plane_remains_alive() {
    let f = Fixture::new();
    let grant = f.grant();
    let permit = f.endpoint.accept(&f.route, f.request(grant, 1)).unwrap();
    drop(f.authority);
    assert!(permit.execute(f.target, || ()).is_err());
}

#[test]
fn another_authority_cannot_register_a_route_or_target_for_this_one() {
    let f = Fixture::new();
    let other = Fixture::new();
    assert!(f
        .authority
        .prepare(&other.route, vec![f.target], Permission::Read, 60, None)
        .is_err());
    assert!(f.endpoint.list(&other.route).is_err());
}
