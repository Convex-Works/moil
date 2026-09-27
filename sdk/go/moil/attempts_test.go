package moil_test

import (
	"errors"
	"slices"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

func retries(evs []moil.Event) []moil.ErrorCode {
	var codes []moil.ErrorCode
	for _, ev := range evs {
		if ev.Kind == moil.EventRetrying {
			codes = append(codes, ev.Err.Code)
		}
	}
	return codes
}

// A machine that goes silent loses the attempt when its lease runs out; it
// is told to stop, and the job moves to another machine.
func TestLeaseExpiryMovesTheJob(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = 150 * time.Millisecond })
	bundle := loadBundle(t, srv, "minimal")
	hung := moiltest.Pair(t, srv, "alice")
	hung.Approve(bundle)
	hung.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	first := hung.NextAttempt()
	first.Log("info", "starting")
	first.GoSilent()

	healthy := moiltest.Pair(t, srv, "alice")
	healthy.Approve(bundle)
	healthy.Connect()
	second := healthy.NextAttempt()
	if second.Number != 2 {
		t.Fatalf("the retry is attempt %d, want 2", second.Number)
	}
	first.WaitCancelled()
	second.Progress(1, "")
	second.Succeed(nil)
	res := succeeded(t, run)
	if res.Machine != healthy.ID() || res.Attempt != 2 {
		t.Fatalf("result from %s attempt %d", res.Machine, res.Attempt)
	}
	if got := retries(events(t, run)); !slices.Equal(got, []moil.ErrorCode{moil.CodeLeaseExpired}) {
		t.Fatalf("retries %v", got)
	}
}

// Events and renewals keep a lease alive for longer than its TTL.
func TestRenewalsKeepAnAttemptAlive(t *testing.T) {
	const lease = 300 * time.Millisecond
	srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = lease })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	a := m.NextAttempt()
	// Word from the machine every 10ms, for three leases, leaves each
	// lease far more slack than a busy test machine needs.
	for start := time.Now(); time.Since(start) < 3*lease; time.Sleep(10 * time.Millisecond) {
		if time.Since(start) < 3*lease/2 {
			a.Log("info", "working")
		} else {
			m.Send(map[string]any{"type": "renew", "job_id": a.JobID, "attempt": a.Number})
		}
	}
	a.Succeed(nil)
	if res := succeeded(t, run); res.Attempt != 1 {
		t.Fatalf("the attempt was replaced; result from attempt %d", res.Attempt)
	}
}

// A lease keeps running while its machine is away. If the machine doesn't
// come back in time, the job moves on, and the machine drops the attempt
// when it does come back.
func TestLeasesRunOutWhileDisconnected(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = 150 * time.Millisecond })
	bundle := loadBundle(t, srv, "minimal")
	away := moiltest.Pair(t, srv, "alice")
	away.Approve(bundle)
	away.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	first := away.NextAttempt()
	away.Disconnect()
	eventually(t, "the lease to run out", func() bool { return len(retries(eventsSoFar(run))) == 1 })
	if got := retries(eventsSoFar(run)); got[0] != moil.CodeLeaseExpired {
		t.Fatalf("the attempt failed with %s", got[0])
	}

	away.Connect() // the service no longer lists the attempt
	if !first.Abandoned() {
		t.Fatal("the machine still holds an attempt its lease ended")
	}
	second := away.NextAttempt()
	second.Succeed(nil)
	if res := succeeded(t, run); res.Attempt != 2 {
		t.Fatalf("result from attempt %d", res.Attempt)
	}
}

// The service acknowledges every done, even for an attempt that's no
// longer current, so the machine can let go of it, and the stale outcome
// changes nothing.
func TestStaleDonesAreAcknowledgedAndIgnored(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = 150 * time.Millisecond })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	hung := m.NextAttempt()
	hung.GoSilent()
	hung.WaitCancelled() // the lease ran out, and the service gave up on it
	other := moiltest.Pair(t, srv, "alice")
	other.Approve(bundle)
	other.Connect()
	second := other.NextAttempt()

	hung.Succeed(map[string]any{"late": true}) // the hung attempt ends after all
	hung.WaitAcked()
	if run.State() != moil.Running {
		t.Fatalf("a stale done changed the job: %s", run.State())
	}
	second.Succeed(nil)
	if res := succeeded(t, run); res.Attempt != 2 || res.Meta != nil {
		t.Fatalf("result from attempt %d with meta %s", res.Attempt, res.Meta)
	}
}

// A machine that drops its connection mid-attempt resumes it: the service
// says which messages it has, the machine replays the rest, and replays of
// messages already processed are ignored.
func TestReconnectResumesTheAttempt(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	a := m.NextAttempt()
	a.Log("info", "one")
	a.Log("info", "two")
	m.Sync()
	m.Disconnect()
	a.Log("info", "three") // kept while offline
	m.Connect()            // replays "three" only
	m.Send(map[string]any{"type": "event", "job_id": a.JobID, "attempt": 1, "seq": 2,
		"event": map[string]any{"v": 1, "type": "log", "message": "two, again"}})
	m.Send(map[string]any{"type": "event", "job_id": a.JobID, "attempt": 1, "seq": 3,
		"event": map[string]any{"v": 1, "type": "log", "message": "three, again"}})
	a.Succeed(nil)
	res := succeeded(t, run)
	if res.Attempt != 1 {
		t.Fatalf("the attempt was replaced: result from attempt %d", res.Attempt)
	}
	var logs []string
	for _, ev := range events(t, run) {
		if ev.Kind == moil.EventLog {
			logs = append(logs, ev.Message)
		}
	}
	if !slices.Equal(logs, []string{"one", "two", "three"}) {
		t.Fatalf("logs %q: each seq must be processed exactly once", logs)
	}
	a.WaitAcked()
}

// An attempt that ends while its machine is offline is reported when the
// machine comes back.
func TestDoneWhileOfflineIsDeliveredOnReconnect(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	a := m.NextAttempt()
	m.Disconnect()
	a.Succeed(map[string]int{"n": 1})
	if run.State() != moil.Running {
		t.Fatalf("state %s while the machine is away", run.State())
	}
	m.Connect()
	if res := succeeded(t, run); string(res.Meta) != `{"n":1}` {
		t.Fatalf("meta %s", res.Meta)
	}
	a.WaitAcked()
}

// A machine that comes back without an attempt it was running (the app
// restarted) loses it, and the job is retried.
func TestAttemptsForgottenByTheMachineAreLost(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	first := m.NextAttempt()
	m.Crash()
	if !first.Abandoned() {
		t.Fatal("a crash must abandon the attempt")
	}
	m.Connect()
	second := m.NextAttempt() // not required to move: this is the only machine
	if second.Number != 2 {
		t.Fatalf("retry is attempt %d", second.Number)
	}
	second.Succeed(nil)
	succeeded(t, run)
	if got := retries(events(t, run)); !slices.Equal(got, []moil.ErrorCode{moil.CodeLost}) {
		t.Fatalf("retries %v", got)
	}
}

// Retryable failures are retried, preferably on a machine that hasn't
// failed the job yet, up to MaxAttempts; then the last error is the job's.
func TestRetriesAreLimited(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.MaxAttempts = 2 })
	bundle := loadBundle(t, srv, "minimal")
	flaky := moiltest.Pair(t, srv, "alice", moiltest.WithName("flaky"))
	flaky.Approve(bundle)
	flaky.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	flaky.NextOffer()
	flaky.NextAttempt()

	other := moiltest.Pair(t, srv, "alice", moiltest.WithName("other"))
	other.Approve(bundle)
	other.Connect()
	// The flaky machine restarts and loses the attempt. Both machines are
	// idle for the retry, and the flaky one bids first, but the other
	// hasn't failed the job yet.
	release := orderBids(t, flaky, other)
	flaky.Crash()
	flaky.Connect()
	release(run.ID())
	second := other.NextAttempt()
	if second.Number != 2 {
		t.Fatalf("attempt %d", second.Number)
	}
	second.ScriptError("CUDA out of memory", true)

	_, err := wait(t, run)
	var jerr *moil.JobError
	if !errors.As(err, &jerr) {
		t.Fatalf("err = %v, want a *JobError", err)
	}
	if jerr.Code != moil.CodeScriptError || jerr.Message != "CUDA out of memory" || !jerr.Retryable || jerr.Attempt != 2 || jerr.Machine != other.ID() {
		t.Fatalf("job error %+v", jerr)
	}
	if run.State() != moil.Failed {
		t.Fatalf("state %s", run.State())
	}
	if got := retries(events(t, run)); !slices.Equal(got, []moil.ErrorCode{moil.CodeLost}) {
		t.Fatalf("retries %v", got)
	}
}

// A failure that can't succeed elsewhere ends the job at once, with the
// details the machine sent.
func TestNonRetryableFailureEndsTheJob(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	exit := 3
	m.NextAttempt().FailWith(moil.JobError{Code: moil.CodeExit, Message: "exited with status 3", ExitCode: &exit, StderrTail: "Traceback…"})
	_, err := wait(t, run)
	var jerr *moil.JobError
	if !errors.As(err, &jerr) || jerr.Code != moil.CodeExit || jerr.ExitCode == nil || *jerr.ExitCode != 3 || jerr.StderrTail != "Traceback…" || jerr.Attempt != 1 {
		t.Fatalf("err = %#v", err)
	}
}

// Attempts that never started (the machine was busy or lacked the bundle)
// don't count toward the limit, and the machine isn't handed the job again
// until it reports a change, so a confused machine can't spin the
// scheduler.
func TestNeverStartedAttemptsAreFree(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.MaxAttempts = 1 })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.RefuseNext(1, moil.CodeBusy)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	eventually(t, "the refusal", func() bool { return len(retries(eventsSoFar(run))) == 1 })
	m.Sync()
	if n := len(m.Offers()); n != 1 {
		t.Fatalf("after a refusal: %d offers; the job must wait for the machine to report a change", n)
	}

	m.RefuseNext(1, moil.CodeNotApproved)
	m.SetState(moil.Idle) // done with whatever kept it busy
	m.Sync()
	if n := len(m.Offers()); n != 2 {
		t.Fatalf("%d offers after the machine reported idle, want 2", n)
	}
	eventually(t, "the second refusal", func() bool { return len(retries(eventsSoFar(run))) == 2 })
	m.Approve(bundle) // the owner approves again: a change
	a := m.NextAttempt()
	if a.Number != 3 {
		t.Fatalf("attempt %d, want 3", a.Number)
	}
	a.Succeed(nil)
	if res := succeeded(t, run); res.Attempt != 3 {
		t.Fatalf("result from attempt %d", res.Attempt)
	}
	if got := retries(events(t, run)); !slices.Equal(got, []moil.ErrorCode{moil.CodeBusy, moil.CodeNotApproved}) {
		t.Fatalf("retries %v", got)
	}
}

// Cancel ends a waiting job at once, and a running one once its machine
// confirms, or at once if the machine is gone.
func TestCancel(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")

	t.Run("waiting", func(t *testing.T) {
		run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("nobody")})
		run.Cancel()
		if _, err := wait(t, run); !errors.Is(err, moil.ErrCancelled) || run.State() != moil.Cancelled {
			t.Fatalf("err %v, state %s", err, run.State())
		}
		run.Cancel() // cancelling again does nothing
	})

	t.Run("running", func(t *testing.T) {
		m := moiltest.Pair(t, srv, "alice")
		m.Approve(bundle)
		m.Connect()
		run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("alice")})
		a := m.NextAttempt()
		run.Cancel()
		a.WaitCancelled() // the machine answers with a cancelled done
		if _, err := wait(t, run); !errors.Is(err, moil.ErrCancelled) {
			t.Fatalf("err %v", err)
		}
		a.WaitAcked()
	})

	t.Run("machine offline", func(t *testing.T) {
		m := moiltest.Pair(t, srv, "carol")
		m.Approve(bundle)
		m.Connect()
		run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("carol")})
		a := m.NextAttempt()
		m.Disconnect()
		eventually(t, "the machine to go offline", func() bool { return !machine(t, srv, m.ID()).Online() })
		run.Cancel()
		if _, err := wait(t, run); !errors.Is(err, moil.ErrCancelled) {
			t.Fatalf("err %v", err)
		}
		m.Connect() // the service no longer lists the attempt, so the machine drops it
		if !a.Abandoned() {
			t.Fatal("the machine still holds the cancelled attempt")
		}
	})

	t.Run("offers out", func(t *testing.T) {
		m := moiltest.Pair(t, srv, "erin")
		m.Approve(bundle)
		answered, bid := make(chan struct{}), make(chan struct{})
		m.OnOffer(func(moiltest.Offer) moiltest.Answer { close(answered); <-bid; return moiltest.Bid })
		m.Connect()
		run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("erin")})
		<-answered // the offer is out; the machine is thinking it over
		run.Cancel()
		if _, err := wait(t, run); !errors.Is(err, moil.ErrCancelled) {
			t.Fatalf("err %v", err)
		}
		close(bid)
		m.OnOffer(func(moiltest.Offer) moiltest.Answer { return moiltest.Bid })
		next := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("erin")})
		a := m.NextAttempt() // the late bid won nothing; the machine is free
		if a.JobID != next.ID() {
			t.Fatalf("the cancelled job was assigned")
		}
		a.Succeed(nil)
		succeeded(t, next)
	})

	t.Run("attempt ended first", func(t *testing.T) {
		m := moiltest.Pair(t, srv, "frank")
		m.Approve(bundle)
		m.Connect()
		// The machine doesn't see the request before its attempts end:
		// a success stands, a failure ends the job as cancelled.
		won := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("frank")})
		a := m.NextAttempt()
		a.GoSilent()
		won.Cancel()
		a.WaitCancelled()
		a.Succeed(nil)
		if _, err := wait(t, won); err != nil {
			t.Fatalf("an attempt that succeeded before seeing the cancel: %v", err)
		}
		lost := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("frank")})
		b := m.NextAttempt()
		b.GoSilent()
		lost.Cancel()
		b.WaitCancelled()
		b.Fail(moil.CodeInterrupted, "the owner stopped it")
		if _, err := wait(t, lost); !errors.Is(err, moil.ErrCancelled) {
			t.Fatalf("an attempt that failed before seeing the cancel: %v", err)
		}
	})

	t.Run("machine hung", func(t *testing.T) {
		srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = 100 * time.Millisecond })
		bundle := loadBundle(t, srv, "minimal")
		m := moiltest.Pair(t, srv, "dave")
		m.Approve(bundle)
		m.Connect()
		run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
		a := m.NextAttempt()
		a.GoSilent()
		run.Cancel()
		a.WaitCancelled()
		// No answer comes; the lease ends it, cancelled rather than retried.
		if _, err := wait(t, run); !errors.Is(err, moil.ErrCancelled) {
			t.Fatalf("err %v", err)
		}
		if got := retries(events(t, run)); len(got) != 0 {
			t.Fatalf("a cancelled job was retried: %v", got)
		}
	})
}

// Removing a machine fails its attempt at once, so the job moves on
// without waiting for the lease.
func TestRemovingAMachineMovesItsJob(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = time.Hour })
	bundle := loadBundle(t, srv, "minimal")
	old := moiltest.Pair(t, srv, "alice")
	old.Approve(bundle)
	old.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	old.NextAttempt()
	replacement := moiltest.Pair(t, srv, "alice")
	replacement.Approve(bundle)
	replacement.Connect()
	if err := srv.RemoveMachine(t.Context(), old.ID()); err != nil {
		t.Fatal(err)
	}
	replacement.NextAttempt().Succeed(nil)
	if res := succeeded(t, run); res.Machine != replacement.ID() {
		t.Fatalf("ran on %s", res.Machine)
	}
	if got := retries(events(t, run)); !slices.Equal(got, []moil.ErrorCode{moil.CodeLost}) {
		t.Fatalf("retries %v", got)
	}
}

// A machine that can't read an assignment rejects it. Another attempt
// would be no different, so the job fails at once.
func TestRejectedAssignmentsEndTheJob(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.RefuseNext(1, moil.CodeRejected)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	_, err := wait(t, run)
	var jerr *moil.JobError
	if !errors.As(err, &jerr) || jerr.Code != moil.CodeRejected || jerr.Retryable || jerr.Attempt != 1 || jerr.Machine != m.ID() {
		t.Fatalf("err = %#v, want the machine's rejection of attempt 1", err)
	}
	if got := retries(events(t, run)); len(got) != 0 {
		t.Fatalf("a rejected job was retried: %v", got)
	}
}

// Each code's default retryability is spec §9's; the machine may say
// otherwise when it knows better.
func TestErrorCodesAreRetryableAsTheSpecSays(t *testing.T) {
	for code, retryable := range map[moil.ErrorCode]bool{
		moil.CodeScriptError:   false,
		moil.CodeExit:          false,
		moil.CodeNoResult:      false,
		moil.CodeOutputMissing: false,
		moil.CodeTimeout:       false,
		moil.CodeEnvironment:   true,
		moil.CodeAssetDownload: true,
		moil.CodeAssetMismatch: false,
		moil.CodeInputDownload: true,
		moil.CodeOutputUpload:  true,
		moil.CodeRejected:      false,
		moil.CodeInterrupted:   true,
		moil.CodeBusy:          true,
		moil.CodeNotApproved:   true,
		moil.CodeInternal:      true,
		moil.CodeLeaseExpired:  true,
		moil.CodeLost:          true,
		"from_a_newer_machine": true,
	} {
		if code.Retryable() != retryable {
			t.Errorf("%s.Retryable() = %v, want %v", code, !retryable, retryable)
		}
		if code.NeverStarted() != (code == moil.CodeBusy || code == moil.CodeNotApproved) {
			t.Errorf("%s.NeverStarted() = %v", code, code.NeverStarted())
		}
	}
}

// A machine that keeps bidding and then refusing can't hold a job
// forever: refusals don't count toward MaxAttempts, but the job fails
// after ten of them.
func TestMachinesCantRefuseAJobForever(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.MaxAttempts = 1 })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.RefuseNext(1000, moil.CodeBusy)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	ended := func() bool {
		select {
		case <-run.Done():
			return true
		default:
			return false
		}
	}
	for refusals := 1; !ended(); refusals++ {
		if refusals > 20 {
			t.Fatal("the job is still going after 20 refusals")
		}
		eventually(t, "a refusal", func() bool { return ended() || len(retries(eventsSoFar(run))) >= refusals })
		m.SetState(moil.Idle) // a change, so the job is offered again
	}
	_, err := wait(t, run)
	var jerr *moil.JobError
	if !errors.As(err, &jerr) || jerr.Code != moil.CodeBusy || jerr.Attempt != 10 {
		t.Fatalf("err = %v, want the tenth refusal", err)
	}
	if n := len(retries(events(t, run))); n != 9 {
		t.Fatalf("%d refusals were retried, want 9", n)
	}
}
