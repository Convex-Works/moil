package moil_test

import (
	"context"
	"errors"
	"slices"
	"strconv"
	"strings"
	"testing"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

// Progress is state: a run keeps only the latest of consecutive progress
// events, however many a machine sends, and a reader following along
// still sees it move forward.
func TestProgressReplacesProgress(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})

	followed := make(chan []float64)
	go func() {
		var fractions []float64
		for ev := range run.Events(context.Background()) {
			if ev.Kind == moil.EventProgress {
				fractions = append(fractions, *ev.Fraction)
			}
		}
		followed <- fractions
	}()

	a := m.NextAttempt()
	for i := range 1000 {
		a.Progress(float64(i+1)/1000, "")
	}
	a.Log("info", "halfway")
	a.Progress(0.1, "second pass")
	a.Progress(0.2, "second pass")
	a.Succeed(nil)
	succeeded(t, run)

	var kept []string
	for _, ev := range events(t, run) {
		kept = append(kept, summary(ev))
	}
	want := []string{"queued", "assigned", "progress 1", "log halfway", "progress 0.2"}
	if !slices.Equal(kept, want) {
		t.Errorf("the run kept %q, want %q", kept, want)
	}
	fractions := <-followed
	if !slices.IsSorted(fractions[:len(fractions)-2]) || !slices.Contains(fractions, 1) || fractions[len(fractions)-1] != 0.2 {
		t.Errorf("a reader following along saw the progress %v", fractions)
	}
}

// A run keeps its first MiB of log and phase events; after that it says
// its log is full and drops the rest, but data still arrives.
func TestLogsAreBounded(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	a := m.NextAttempt()
	line := strings.Repeat("x", 32<<10)
	for range 40 {
		a.Log("info", line)
	}
	a.Phase("uploading")
	a.Data("still here")
	a.Succeed(nil)
	succeeded(t, run)

	var logs, full int
	var kinds []moil.EventKind
	for _, ev := range events(t, run) {
		switch {
		case ev.Kind == moil.EventLog && ev.Message == line:
			logs++
		case ev.Kind == moil.EventLog && ev.Level == "warn" && strings.Contains(ev.Message, "log is full"):
			full++
		default:
			kinds = append(kinds, ev.Kind)
		}
	}
	if logs < 30 || logs >= 32 || full != 1 {
		t.Errorf("the run kept %d of 40 32 KiB log lines and %d warnings, want the first MiB of them and one warning", logs, full)
	}
	if want := []moil.EventKind{moil.EventQueued, moil.EventAssigned, moil.EventData}; !slices.Equal(kinds, want) {
		t.Errorf("the run kept %v besides the logs, want %v", kinds, want)
	}
}

// Data is what a run is for, so it's kept, up to Config.MaxDataBytes. An
// attempt that sends more is stopped, and its job fails.
func TestDataIsBounded(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.MaxDataBytes = 4 << 10 })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	a := m.NextAttempt()
	chunk := strings.Repeat("x", 1000) // counts 1002 + 256 bytes
	for range 4 {
		a.Data(chunk)
	}
	a.WaitCancelled()
	if _, err := wait(t, run); !errors.Is(err, moil.ErrTooMuchData) {
		t.Fatalf("Wait = %v, want ErrTooMuchData", err)
	}
	var data int
	for _, ev := range events(t, run) {
		if ev.Kind == moil.EventData {
			data++
		}
	}
	if data != 3 {
		t.Errorf("the run kept %d data events, want the 3 that fit", data)
	}
	if got := retries(events(t, run)); len(got) != 0 {
		t.Errorf("the job was retried: %v", got)
	}
}

// summary describes an event in a few words.
func summary(ev moil.Event) string {
	switch ev.Kind {
	case moil.EventProgress:
		return "progress " + strconv.FormatFloat(*ev.Fraction, 'g', -1, 64)
	case moil.EventLog:
		return "log " + ev.Message
	}
	return string(ev.Kind)
}
