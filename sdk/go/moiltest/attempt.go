package moiltest

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"io"
	"net/http"
	"sync"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/internal/wire"
	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// An Attempt is an attempt the service assigned to a Machine. The test
// plays the script and runtime: it reports events, moves files and ends
// the attempt. Messages are numbered and kept until the service
// acknowledges the end, so an attempt carries on across Disconnect and
// Connect as it does in the app.
//
// Its fields describe the assignment and don't change. Its methods must be
// called from the test goroutine.
type Attempt struct {
	JobID      string
	Number     int
	BundleHash string
	Title      string
	// Params is the job's params, as JSON.
	Params  json.RawMessage
	Inputs  map[string]moil.Download
	Outputs map[string]moil.Upload
	// Lease is how long the service waits between messages; the machine
	// renews it every third of it until GoSilent.
	Lease time.Duration
	// Timeout is the attempt's time limit, or 0.
	Timeout time.Duration

	m *Machine

	// Guarded by m.mu.
	seq       int64
	kept      []keptMessage
	started   bool // the machine accepted the assignment
	finished  bool // done was sent
	abandoned bool
	silent    bool
	uploaded  map[string]wire.FileInfo

	cancelled  chan struct{}
	cancelOnce sync.Once
	acked      chan struct{}
	ended      chan struct{} // closed when the attempt finishes or is abandoned
	endOnce    sync.Once
}

type keptMessage struct {
	seq  int64
	data []byte
}

func newAttempt(m *Machine, msg wire.Assign) *Attempt {
	a := &Attempt{
		JobID:      msg.JobID,
		Number:     msg.Attempt,
		BundleHash: msg.BundleHash,
		Title:      msg.Title,
		Params:     msg.Params,
		Inputs:     make(map[string]moil.Download, len(msg.Inputs)),
		Outputs:    make(map[string]moil.Upload, len(msg.Outputs)),
		Lease:      time.Duration(msg.LeaseMS) * time.Millisecond,
		Timeout:    time.Duration(msg.TimeoutMS) * time.Millisecond,
		m:          m,
		uploaded:   map[string]wire.FileInfo{},
		cancelled:  make(chan struct{}),
		acked:      make(chan struct{}),
		ended:      make(chan struct{}),
	}
	for name, in := range msg.Inputs {
		a.Inputs[name] = moil.Download{URL: in.URL, Headers: in.Headers}
	}
	for name, out := range msg.Outputs {
		a.Outputs[name] = moil.Upload{URL: out.URL, Method: out.Method, Headers: out.Headers}
	}
	return a
}

func (a *Attempt) running() bool { return a.started && !a.finished && !a.abandoned }

// Phase reports that the runtime entered a phase: "preparing",
// "downloading", "running" or "uploading".
func (a *Attempt) Phase(phase string) {
	a.m.t.Helper()
	a.event(wire.JobEvent{Type: "phase", Phase: phase})
}

// Progress reports the script's progress: fraction from 0 to 1, and a
// message ("" for none).
func (a *Attempt) Progress(fraction float64, message string) {
	a.m.t.Helper()
	e := wire.JobEvent{Type: "progress", Fraction: &fraction}
	if message != "" {
		e.Message = &message
	}
	a.event(e)
}

// Log reports a log line at level "debug", "info", "warn" or "error".
func (a *Attempt) Log(level, message string) {
	a.m.t.Helper()
	a.event(wire.JobEvent{Type: "log", Level: level, Message: &message})
}

// Data sends service-specific JSON, as the script's data events do.
func (a *Attempt) Data(payload any) {
	a.m.t.Helper()
	data, err := json.Marshal(payload)
	if err != nil {
		a.m.t.Fatal(err)
	}
	a.event(wire.JobEvent{Type: "data", Payload: data})
}

func (a *Attempt) event(e wire.JobEvent) {
	a.m.t.Helper()
	a.m.mu.Lock()
	defer a.m.mu.Unlock()
	a.mustRunLocked()
	a.sendLocked(wire.Event{Event: e})
}

func (a *Attempt) mustRunLocked() {
	a.m.t.Helper()
	if !a.running() {
		a.m.t.Fatalf("moiltest: attempt %d of %s has already ended", a.Number, a.JobID)
	}
}

// sendLocked numbers an event or done, keeps it until acknowledged and
// sends it if connected.
func (a *Attempt) sendLocked(msg wire.Message) {
	a.seq++
	switch m := msg.(type) {
	case wire.Event:
		m.JobID, m.Attempt, m.Seq = a.JobID, a.Number, a.seq
		msg = m
	case wire.Done:
		m.JobID, m.Attempt, m.Seq = a.JobID, a.Number, a.seq
		msg = m
	}
	data, err := wire.Encode(msg)
	if err != nil {
		a.m.t.Errorf("moiltest: encoding %s: %v", msg.Type(), err)
		return
	}
	a.kept = append(a.kept, keptMessage{seq: a.seq, data: data})
	a.m.writeLocked(data)
}

// Input downloads an input the way the runtime does, and returns its
// bytes. It fails the test unless the download answers 2xx.
func (a *Attempt) Input(name string) []byte {
	a.m.t.Helper()
	in, ok := a.Inputs[name]
	if !ok {
		a.m.t.Fatalf("moiltest: the job has no input %q", name)
	}
	req, err := http.NewRequest(http.MethodGet, in.URL, nil)
	if err != nil {
		a.m.t.Fatal(err)
	}
	for k, v := range in.Headers {
		req.Header.Set(k, v)
	}
	resp, err := a.m.client.Do(req)
	if err != nil {
		a.m.t.Fatalf("moiltest: downloading input %q: %v", name, err)
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(resp.Body)
	if err != nil || resp.StatusCode/100 != 2 {
		a.m.t.Fatalf("moiltest: downloading input %q: %s %v", name, resp.Status, err)
	}
	return data
}

// Output uploads a declared output the way the runtime does: the bytes as
// the body, with the declared method and headers. It fails the test unless
// the upload answers 2xx. Succeed reports every uploaded output.
func (a *Attempt) Output(name string, data []byte) {
	a.m.t.Helper()
	out, ok := a.Outputs[name]
	if !ok {
		a.m.t.Fatalf("moiltest: the job declares no output %q", name)
	}
	method := out.Method
	if method == "" {
		method = http.MethodPut
	}
	req, err := http.NewRequest(method, out.URL, bytes.NewReader(data))
	if err != nil {
		a.m.t.Fatal(err)
	}
	for k, v := range out.Headers {
		req.Header.Set(k, v)
	}
	resp, err := a.m.client.Do(req)
	if err != nil {
		a.m.t.Fatalf("moiltest: uploading output %q: %v", name, err)
	}
	resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		a.m.t.Fatalf("moiltest: uploading output %q: %s", name, resp.Status)
	}
	sum := sha256.Sum256(data)
	a.m.mu.Lock()
	defer a.m.mu.Unlock()
	a.uploaded[name] = wire.FileInfo{SizeBytes: int64(len(data)), SHA256: hex.EncodeToString(sum[:])}
}

// Succeed ends the attempt successfully, with meta (nil for none) and
// every output uploaded with Output.
func (a *Attempt) Succeed(meta any) {
	a.m.t.Helper()
	var raw json.RawMessage
	if meta != nil {
		var err error
		if raw, err = json.Marshal(meta); err != nil {
			a.m.t.Fatal(err)
		}
	}
	a.m.mu.Lock()
	defer a.m.mu.Unlock()
	a.mustRunLocked()
	files := make(map[string]wire.FileInfo, len(a.uploaded))
	for name, f := range a.uploaded {
		files[name] = f
	}
	a.doneLocked(wire.Done{Outcome: wire.OutcomeSucceeded, Result: &wire.Result{Meta: raw, Files: files}})
}

// Fail ends the attempt with an error code, retryable as the spec says for
// that code (see moil.ErrorCode.Retryable).
func (a *Attempt) Fail(code moil.ErrorCode, message string) {
	a.m.t.Helper()
	a.FailWith(moil.JobError{Code: code, Message: message, Retryable: code.Retryable()})
}

// ScriptError ends the attempt as a script that sent an error event does.
func (a *Attempt) ScriptError(message string, retryable bool) {
	a.m.t.Helper()
	a.FailWith(moil.JobError{Code: moil.CodeScriptError, Message: message, Retryable: retryable})
}

// FailWith ends the attempt with e's code, message, retryability, exit
// code and stderr tail.
func (a *Attempt) FailWith(e moil.JobError) {
	a.m.t.Helper()
	a.m.mu.Lock()
	defer a.m.mu.Unlock()
	a.mustRunLocked()
	a.doneLocked(wire.Done{Outcome: wire.OutcomeFailed, Error: &wire.JobError{
		Code: string(e.Code), Message: e.Message, Retryable: e.Retryable, ExitCode: e.ExitCode, StderrTail: e.StderrTail,
	}})
}

func (a *Attempt) doneLocked(d wire.Done) {
	a.sendLocked(d)
	a.finished = true
	a.endOnce.Do(func() { close(a.ended) })
	a.m.reportStateLocked()
	a.m.changedLocked()
}

func (a *Attempt) abandonLocked() {
	a.abandoned = true
	a.endOnce.Do(func() { close(a.ended) })
}

// GoSilent makes the machine stop sending anything for the attempt, as if
// it hung: no renewals, and no answer to cancel. The service's lease on
// the attempt runs out.
func (a *Attempt) GoSilent() {
	a.m.mu.Lock()
	defer a.m.mu.Unlock()
	a.silent = true
}

// WaitCancelled waits until the service cancels the attempt. The machine
// answers with a cancelled done by itself, unless the attempt went silent.
func (a *Attempt) WaitCancelled() {
	a.m.t.Helper()
	a.wait(a.cancelled, "the service to cancel")
}

// WaitAcked waits until the service acknowledges the attempt's end.
func (a *Attempt) WaitAcked() {
	a.m.t.Helper()
	a.wait(a.acked, "the service to acknowledge the end of")
}

func (a *Attempt) wait(ch <-chan struct{}, what string) {
	a.m.t.Helper()
	select {
	case <-ch:
	case <-time.After(a.m.timeout):
		a.m.t.Fatalf("moiltest: waited %v for %s attempt %d of %s", a.m.timeout, what, a.Number, a.JobID)
	}
}

// Cancelled reports whether the service has cancelled the attempt.
func (a *Attempt) Cancelled() bool {
	select {
	case <-a.cancelled:
		return true
	default:
		return false
	}
}

// Abandoned reports whether the machine dropped the attempt: the service
// left it out of welcome's resume list, or the machine crashed.
func (a *Attempt) Abandoned() bool {
	a.m.mu.Lock()
	defer a.m.mu.Unlock()
	return a.abandoned
}

// renewLoop renews the lease every third of it while the attempt runs.
func (a *Attempt) renewLoop() {
	defer a.m.goroutines.Done()
	t := time.NewTicker(max(a.Lease/3, time.Millisecond))
	defer t.Stop()
	for {
		select {
		case <-a.ended:
			return
		case <-a.m.stopped:
			return
		case <-t.C:
			a.m.mu.Lock()
			if a.running() && !a.silent {
				a.m.sendLocked(wire.Renew{JobID: a.JobID, Attempt: a.Number})
			}
			a.m.mu.Unlock()
		}
	}
}
