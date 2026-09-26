package e2e

import (
	"context"
	"encoding/json"
	"errors"
	"slices"
	"strconv"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// When the agent crashes mid-job, the service notices once the attempt's
// lease runs out and retries the job. The processes the crash left behind
// keep running until the agent starts again and cleans up after itself;
// then the job runs to the end.
func TestAJobSurvivesItsMachineCrashing(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t, withLease(2*time.Second))
	never := newGate(t)
	run := svc.Submit(moil.Job{Params: probeAttempts(
		[]step{do.Spawn(), do.WaitFor(never)},
		[]step{do.Result(nil)},
	)})
	p := awaitPids(t, run, 1)

	m.Kill()
	if e := awaitRetry(t, run, 1); e.Code != moil.CodeLeaseExpired {
		t.Fatalf("attempt 1 failed with %s, want %s", e.Code, moil.CodeLeaseExpired)
	}
	for _, pid := range p.all() {
		if !alive(pid) {
			t.Fatalf("process %d of the crashed attempt ended by itself; the test can't tell whether the agent cleans up", pid)
		}
	}

	m.Start()
	awaitGone(t, "the processes the crash left behind", p.all()...)
	m.Logged("cleaned up 1 attempts left over from a crash")
	if res := succeed(t, run); res.Attempt != 2 || res.Machine != m.ID() {
		t.Errorf("the job succeeded on attempt %d on %s, want attempt 2 on %s", res.Attempt, res.Machine, m.ID())
	}
}

// When its owner stops the agent mid-job, it stops the job and tells the
// service at once, long before the lease would have run out, so the job
// can go elsewhere. Here it goes back to the same machine once the agent
// is back.
func TestStoppingTheAgentHandsTheJobBack(t *testing.T) {
	t.Parallel()
	lease := time.Minute
	svc, m := lentMachine(t, withLease(lease))
	never := newGate(t)
	run := svc.Submit(moil.Job{Params: probeAttempts(
		[]step{do.Spawn(), do.WaitFor(never)},
		[]step{do.Result(nil)},
	)})
	p := awaitPids(t, run, 1)

	stopped := time.Now()
	m.Stop()
	e := awaitRetry(t, run, 1)
	if e.Code != moil.CodeInterrupted || !e.Retryable {
		t.Fatalf("attempt 1 failed with %+v, want a retryable %s", e, moil.CodeInterrupted)
	}
	if took := time.Since(stopped); took > lease/6 {
		t.Errorf("the service learned of the interruption after %v, with a lease of %v", took, lease)
	}
	awaitGone(t, "the stopped job's processes", p.all()...)

	m.Start()
	if res := succeed(t, run); res.Attempt != 2 || res.Machine != m.ID() {
		t.Errorf("the job succeeded on attempt %d on %s, want attempt 2 on %s", res.Attempt, res.Machine, m.ID())
	}
}

// When the network drops mid-job, the job carries on, and the agent
// reconnects and resumes it: the service receives what the script sent
// while the machine was offline, every event exactly once and in order,
// and the attempt succeeds.
func TestAJobSurvivesTheNetworkDropping(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t, withLease(20*time.Second))
	dropped, sentOffline, back := newGate(t), newGate(t), newGate(t)
	run := svc.Submit(moil.Job{Params: probe(
		do.Count(1, 5),
		do.WaitFor(dropped),
		do.Count(6, 10),
		do.Open(sentOffline),
		do.WaitFor(back),
		do.Count(11, 15),
		do.Result(nil),
	)})
	awaitCount(t, run, 5)

	connections := m.link.Connections()
	m.link.Cut()
	eventually(t, "the service to see the machine go offline", func() bool { return !svc.Machine(m.ID()).Online() })
	dropped.Open(t)
	sentOffline.Await(t, "the script to send events while the machine is offline")

	m.link.Restore()
	awaitCount(t, run, 10)
	if m.link.Connections() == connections {
		t.Fatal("the events sent offline arrived without the agent reconnecting")
	}
	back.Open(t)

	if res := succeed(t, run); res.Attempt != 1 {
		t.Errorf("the job succeeded on attempt %d, want 1", res.Attempt)
	}
	var counted []int
	for _, ev := range history(run) {
		var payload struct{ N int }
		if ev.Kind == moil.EventData && json.Unmarshal(ev.Data, &payload) == nil {
			counted = append(counted, payload.N)
		}
		if ev.Kind == moil.EventRetrying {
			t.Errorf("attempt %d failed: %v", ev.Attempt, ev.Err)
		}
	}
	want := []int{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15}
	if !slices.Equal(counted, want) {
		t.Errorf("the service received the numbers %v, want %v", counted, want)
	}
}

// A service keeps its jobs in memory, so restarting it forgets them.
// Machines then drop the attempts they were running for it, and the
// service resubmits, by the same ID, what must finish.
func TestAServiceRestartDropsTheJobsItForgot(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	never := newGate(t)
	job := moil.Job{ID: "meeting-42-transcript", Params: probe(do.Spawn(), do.WaitFor(never))}
	run := svc.Submit(job)
	p := awaitPids(t, run, 1)

	svc.Restart()
	if _, err := finish(t, run); !errors.Is(err, moil.ErrClosed) {
		t.Errorf("the restart ended the job with %v, want %v", err, moil.ErrClosed)
	}
	m.Logged("no longer expects meeting-42-transcript attempt 1")
	awaitGone(t, "the forgotten attempt's processes", p.all()...)

	job.Params = probe(do.Result(nil))
	if res := succeed(t, svc.Submit(job)); res.Machine != m.ID() {
		t.Errorf("the resubmitted job ran on %s, not %s", res.Machine, m.ID())
	}
}

// reconnectWindow is long enough for an agent to try to reconnect after
// losing its channel: it tries again within a second (spec §6.1).
const reconnectWindow = 2 * time.Second

// Removing a machine from the service cuts it off: its job moves on
// without it, it stops working on the job, and it doesn't try to
// reconnect, telling its owner to pair it again instead.
func TestRemovingAMachineCutsItOff(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	never := newGate(t)
	run := svc.Submit(moil.Job{Params: probe(do.Spawn(), do.WaitFor(never))})
	p := awaitPids(t, run, 1)
	connections := m.link.Connections()

	if err := svc.sdk.RemoveMachine(context.Background(), m.ID()); err != nil {
		t.Fatal(err)
	}
	if e := awaitRetry(t, run, 1); e.Code != moil.CodeLost {
		t.Errorf("attempt 1 failed with %s, want %s", e.Code, moil.CodeLost)
	}
	m.Logged("removed this machine; pair it again")
	awaitGone(t, "the removed machine's job processes", p.all()...)

	time.Sleep(reconnectWindow)
	if n := m.link.Connections() - connections; n != 0 {
		t.Errorf("the removed machine connected %d more times", n)
	}
	if _, err := svc.sdk.Machine(context.Background(), m.ID()); !errors.Is(err, moil.ErrNotFound) {
		t.Errorf("the service still knows the removed machine: %v", err)
	}
}

// awaitCount waits until the probe's count reaches n.
func awaitCount(t *testing.T, run *moil.Run, n int) {
	t.Helper()
	awaitEvent(t, run, "the probe to count to "+strconv.Itoa(n), func(ev moil.Event) bool {
		var payload struct{ N int }
		return ev.Kind == moil.EventData && json.Unmarshal(ev.Data, &payload) == nil && payload.N == n
	})
}
