package moil

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"sync"
	"time"
)

// A Job is a unit of work for a machine: run a bundle with these params,
// these input files and these output destinations.
type Job struct {
	// ID identifies the job, ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$. It is
	// optional; one is generated if empty. Submitting a job whose ID the
	// Server already knows returns the existing Run, so a service can
	// derive IDs from its own records (e.g. "meeting-42-transcript") and
	// resubmit safely.
	ID string
	// Bundle is the code to run. It must have been added with AddBundle.
	Bundle *Bundle
	// Title is an optional label (at most 200 characters) machines may
	// show their owners, e.g. the meeting's name.
	Title string
	// Params is handed to the script as JSON. It is marshaled once, at
	// Submit; nil becomes null.
	Params any
	// Inputs are the files the runtime downloads before starting the
	// script, by file name (^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$).
	Inputs map[string]Download
	// Outputs are the files the script may produce and where the runtime
	// uploads them, by file name.
	Outputs map[string]Upload
	// Prepare, if set, makes each attempt's inputs and outputs as the
	// attempt is assigned, instead of Inputs and Outputs. Use it for
	// pre-signed URLs, which could expire while the job waits for a
	// machine or between attempts. It runs on its own goroutine before the
	// machine hears of the attempt; ctx ends if the job does. An error
	// ends the job, and Run.Wait returns it.
	Prepare func(ctx context.Context, a Assignment) (inputs map[string]Download, outputs map[string]Upload, err error)
	// Eligible is the service's policy for which machines may run the
	// job, for example OwnedBy(room.OwnerID). It is required: machine
	// owners can see a job's inputs, and only the service knows who may.
	// Use AnyMachine to allow every paired machine.
	//
	// The Server calls Eligible from its scheduler, possibly many times.
	// It must be fast and must not call the Server.
	Eligible func(Machine) bool
	// Prefer optionally ranks the machines that bid for the job: the
	// highest score wins, ties go to the earliest bid. Without it the
	// earliest bid wins. When retrying, machines that already failed the
	// job rank below all others. Like Eligible, it must be fast and must
	// not call the Server.
	Prefer func(Machine) int
	// Timeout limits each attempt's running time on the machine. Zero
	// means no limit.
	Timeout time.Duration
	// MaxAttempts overrides Config.MaxAttempts for this job when positive.
	MaxAttempts int
}

// An Assignment is an attempt about to be sent to a machine.
type Assignment struct {
	JobID   string
	Attempt int
	Machine Machine
}

// A Download is where the runtime fetches an input from, with GET. URLs are
// typically pre-signed; they must stay valid until the job runs.
type Download struct {
	URL     string
	Headers map[string]string
}

// An Upload is where the runtime sends an output to.
type Upload struct {
	URL string
	// Method is "PUT" (the default when empty) or "POST".
	Method  string
	Headers map[string]string
}

// ErrorCode says why an attempt failed (spec §9).
type ErrorCode string

// The error codes of spec §9. Machines report all but the last two.
const (
	CodeScriptError   ErrorCode = "script_error"   // the script sent an error event
	CodeExit          ErrorCode = "exit"           // nonzero exit, or killed by a signal
	CodeNoResult      ErrorCode = "no_result"      // exited without a result
	CodeOutputMissing ErrorCode = "output_missing" // the result named an undeclared or absent file
	CodeTimeout       ErrorCode = "timeout"        // ran past Job.Timeout
	CodeEnvironment   ErrorCode = "environment"    // uv couldn't create the environment
	CodeAssetDownload ErrorCode = "asset_download" // an asset couldn't be downloaded
	CodeAssetMismatch ErrorCode = "asset_mismatch" // an asset's hash didn't match the manifest
	CodeInputDownload ErrorCode = "input_download" // an input couldn't be downloaded
	CodeOutputUpload  ErrorCode = "output_upload"  // an output couldn't be uploaded
	CodeInterrupted   ErrorCode = "interrupted"    // stopped by the machine's owner, or the app quit
	CodeBusy          ErrorCode = "busy"           // assigned while not idle; never started
	CodeNotApproved   ErrorCode = "not_approved"   // bundle not approved or not stored; never started
	CodeInternal      ErrorCode = "internal"       // a bug in the machine's runtime
	CodeLeaseExpired  ErrorCode = "lease_expired"  // the machine stopped reporting and its lease ran out
	CodeLost          ErrorCode = "lost"           // the machine came back without the attempt, or was removed
)

// Retryable reports whether the spec says another attempt could succeed
// after an error with this code. For CodeScriptError the script decides;
// this reports false, the default when it doesn't say.
func (c ErrorCode) Retryable() bool {
	switch c {
	case CodeScriptError, CodeExit, CodeNoResult, CodeOutputMissing, CodeTimeout, CodeEnvironment, CodeAssetMismatch:
		return false
	}
	return true
}

// NeverStarted reports whether an attempt that failed with this code never
// started, so it doesn't count toward a job's attempt limit (spec §7.2).
func (c ErrorCode) NeverStarted() bool { return c == CodeBusy || c == CodeNotApproved }

// A JobError is why an attempt failed. Run.Wait returns the last attempt's
// error when a job fails.
type JobError struct {
	Code    ErrorCode
	Message string
	// Retryable says whether another attempt could succeed.
	Retryable bool
	// ExitCode is the script's exit status, if it exited normally.
	ExitCode *int
	// StderrTail is the end of the script's stderr (at most 8 KiB), if a
	// process ran.
	StderrTail string
	// Machine and Attempt say which attempt failed.
	Machine string
	Attempt int
}

// Error describes the failure with the attempt and machine it happened on.
func (e *JobError) Error() string {
	return fmt.Sprintf("moil: attempt %d on machine %s failed: %s: %s", e.Attempt, e.Machine, e.Code, e.Message)
}

// ErrCancelled is returned by Run.Wait for a job that was cancelled.
var ErrCancelled = errors.New("moil: job cancelled")

// ErrClosed is returned for work the Server can no longer do because it
// was closed.
var ErrClosed = errors.New("moil: server closed")

// A Result is what a succeeded job produced.
type Result struct {
	// Meta is the script's result metadata, untouched; nil if it sent
	// none.
	Meta json.RawMessage
	// Files describes every uploaded output, by name.
	Files map[string]File
	// Machine and Attempt say which attempt succeeded.
	Machine string
	Attempt int
}

// A File describes one output as uploaded by the machine.
type File struct {
	SizeBytes int64
	SHA256    string
}

// EventKind says what an Event reports.
type EventKind string

// Event kinds. Queued, Assigned and Retrying come from the Server; Phase,
// Progress, Log and Data are forwarded from the attempt's machine.
const (
	EventQueued   EventKind = "queued"   // submitted; waiting for a machine
	EventAssigned EventKind = "assigned" // Attempt was assigned to Machine
	EventRetrying EventKind = "retrying" // Attempt failed with Err; the job waits for a machine again
	EventPhase    EventKind = "phase"    // the runtime entered Phase
	EventProgress EventKind = "progress" // the script reported Fraction and/or Message
	EventLog      EventKind = "log"      // the script logged Message at Level
	EventData     EventKind = "data"     // the script sent Data, service-specific JSON
)

// An Event is one step of a Run. Which fields are set depends on Kind.
type Event struct {
	Kind EventKind
	Time time.Time
	// Attempt and Machine say which attempt the event is about; they're
	// zero for EventQueued.
	Attempt int
	Machine string
	// Phase is "preparing", "downloading", "running" or "uploading".
	Phase string
	// Fraction is the progress from 0 to 1, when the script gave one.
	Fraction *float64
	// Message is the progress or log message.
	Message string
	// Level is the log level: "debug", "info", "warn" or "error".
	Level string
	// Data is the payload of EventData, untouched.
	Data json.RawMessage
	// Err is why the attempt failed, for EventRetrying.
	Err *JobError
}

// RunState is where a Run is.
type RunState string

// Run states. A Run moves between Queued and Running as attempts come and
// go, and ends in Succeeded, Failed or Cancelled.
const (
	Queued    RunState = "queued"
	Running   RunState = "running"
	Succeeded RunState = "succeeded"
	Failed    RunState = "failed"
	Cancelled RunState = "cancelled"
)

// A Run is a submitted job. Its methods are safe to call from any
// goroutine.
//
// Runs live in memory: when the service restarts, unfinished jobs are
// lost, and machines abandon the attempts they were running for them. A
// service that must finish a job resubmits it, by the same ID, on startup.
type Run struct {
	id     string
	cancel func()

	mu     sync.Mutex
	state  RunState
	events []Event
	wake   chan struct{} // closed and replaced when something changes
	done   chan struct{} // closed when the run ends
	ended  bool
	result *Result
	err    error
}

func newRun(id string, cancel func()) *Run {
	return &Run{id: id, cancel: cancel, state: Queued, wake: make(chan struct{}), done: make(chan struct{})}
}

// ID is the job's ID.
func (r *Run) ID() string { return r.id }

// State reports where the run is now.
func (r *Run) State() RunState {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.state
}

// Events returns the run's events in order, from the first, then each new
// one as it happens. The sequence ends after the run ends and every event
// has been yielded, or when ctx is done. Each call starts from the first
// event, and the Server never drops or waits for events: a slow reader only
// delays itself.
func (r *Run) Events(ctx context.Context) iter.Seq[Event] {
	return func(yield func(Event) bool) {
		next := 0
		for {
			r.mu.Lock()
			// Events are only ever appended, so the elements of this
			// slice never change after it's taken.
			pending := r.events[next:]
			ended := r.ended
			wake := r.wake
			r.mu.Unlock()
			for _, ev := range pending {
				if !yield(ev) {
					return
				}
			}
			next += len(pending)
			if len(pending) > 0 {
				continue
			}
			if ended {
				return
			}
			select {
			case <-wake:
			case <-ctx.Done():
				return
			}
		}
	}
}

// Done is closed when the run ends.
func (r *Run) Done() <-chan struct{} { return r.done }

// Wait blocks until the run ends or ctx is done. A succeeded job returns
// its Result. A failed one returns a *JobError, or the error from
// Job.Prepare; a cancelled one ErrCancelled, and one the Server gave up on
// when closing ErrClosed.
func (r *Run) Wait(ctx context.Context) (*Result, error) {
	select {
	case <-r.done:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.result, r.err
}

// Cancel asks for the job to stop. A job that is waiting for a machine
// ends at once; a running one ends when its machine confirms, or at once
// if the machine is offline. If the attempt finishes before its machine
// sees the request, the run ends with that attempt's outcome instead.
// Cancelling an ended run does nothing.
func (r *Run) Cancel() { r.cancel() }

// The methods below are called by the scheduler.

func (r *Run) emit(ev Event) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.events = append(r.events, ev)
	close(r.wake)
	r.wake = make(chan struct{})
}

func (r *Run) setState(s RunState) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.state = s
}

func (r *Run) finish(s RunState, res *Result, err error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.state, r.result, r.err, r.ended = s, res, err, true
	close(r.wake)
	r.wake = make(chan struct{})
	close(r.done)
}
