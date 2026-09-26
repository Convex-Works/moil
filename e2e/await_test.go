package e2e

import (
	"context"
	"encoding/json"
	"errors"
	"syscall"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// patience is how long a test waits for something that should happen
// before it gives up. Things happen much sooner when all is well.
const patience = 30 * time.Second

// eventually waits until cond holds, failing the test with what it
// waited for if it doesn't within patience.
func eventually(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(patience)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("gave up waiting for %s", what)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// awaitEvent waits for the first event of run that matches, and returns
// it.
func awaitEvent(t *testing.T, run *moil.Run, what string, match func(moil.Event) bool) moil.Event {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), patience)
	defer cancel()
	for ev := range run.Events(ctx) {
		if match(ev) {
			return ev
		}
	}
	if ctx.Err() != nil {
		t.Fatalf("gave up waiting for %s; the job's events:\n%s", what, describe(history(run)))
	}
	res, err := run.Wait(ctx)
	t.Fatalf("the job ended (%v, %v) without %s; its events:\n%s", res, err, what, describe(history(run)))
	return moil.Event{}
}

// awaitRetry waits for attempt to fail and the job to be retried, and
// returns why the attempt failed.
func awaitRetry(t *testing.T, run *moil.Run, attempt int) *moil.JobError {
	t.Helper()
	ev := awaitEvent(t, run, "a retry", func(ev moil.Event) bool {
		return ev.Kind == moil.EventRetrying && ev.Attempt == attempt
	})
	return ev.Err
}

// finish waits for run to end.
func finish(t *testing.T, run *moil.Run) (*moil.Result, error) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), patience)
	defer cancel()
	res, err := run.Wait(ctx)
	if errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("gave up waiting for job %s to end; its events:\n%s", run.ID(), describe(history(run)))
	}
	return res, err
}

// succeed waits for run to succeed and returns its result.
func succeed(t *testing.T, run *moil.Run) *moil.Result {
	t.Helper()
	res, err := finish(t, run)
	if err != nil {
		t.Fatalf("job %s failed: %v; its events:\n%s", run.ID(), err, describe(history(run)))
	}
	return res
}

// failure waits for run to fail and returns why.
func failure(t *testing.T, run *moil.Run) *moil.JobError {
	t.Helper()
	res, err := finish(t, run)
	var jobErr *moil.JobError
	if !errors.As(err, &jobErr) {
		t.Fatalf("job %s ended with %v, %v instead of failing; its events:\n%s", run.ID(), res, err, describe(history(run)))
	}
	return jobErr
}

// history returns the events of run so far.
func history(run *moil.Run) []moil.Event {
	ctx, cancel := context.WithCancel(context.Background())
	cancel() // take what's there without waiting for more
	var events []moil.Event
	for ev := range run.Events(ctx) {
		events = append(events, ev)
	}
	return events
}

func describe(events []moil.Event) string {
	var out []byte
	for _, ev := range events {
		line, _ := json.Marshal(ev)
		out = append(out, "  "...)
		out = append(out, line...)
		out = append(out, '\n')
	}
	return string(out)
}

// alive reports whether a process runs. A zombie counts: it's gone once
// its parent reaps it.
func alive(pid int) bool {
	return syscall.Kill(pid, 0) == nil
}

// awaitGone waits until none of pids runs.
func awaitGone(t *testing.T, what string, pids ...int) {
	t.Helper()
	eventually(t, what+" to end", func() bool {
		for _, pid := range pids {
			if alive(pid) {
				return false
			}
		}
		return true
	})
}
