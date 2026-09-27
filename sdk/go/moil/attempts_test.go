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

// Events, renewals and a slow script keep a lease alive for longer than
// its TTL.
func TestRenewalsKeepAnAttemptAlive(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = 90 * time.Millisecond })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	a := m.NextAttempt()
	// The fake renews every 30ms; wait out several lease lengths.
	deadline := time.Now().Add(400 * time.Millisecond)
	for time.Now().Before(deadline) {
		m.Sync()
	}
	a.Succeed(nil)
	if res := succeeded(t, run); res.Attempt != 1 {
		t.Fatalf("the attempt was replaced; result from attempt %d", res.Attempt)
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
