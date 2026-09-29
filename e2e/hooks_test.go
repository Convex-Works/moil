package e2e

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// After an attempt of a bundle succeeds, the owner's hook for that bundle
// runs with its outputs: here a shell script that keeps the transcript
// the job made. It reads the attempt's context on stdin, runs in an empty
// folder that is also its TMPDIR, finds the outputs and nothing else in
// output_dir, has only the environment the spec lists, and what it prints
// goes to its log. The service has heard
// that the job succeeded while the hook still runs, and hears nothing of
// the hook.
func TestAHookTurnsAJobsOutputIntoTheOwners(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	finish := newGate(t)
	hook := m.InstallHook("probe", finish)

	audio := randomBytes(t, 256<<10)
	transcript := svc.Receive()
	meta := `{"speakers": 3, "language": "en"}`
	submitted := time.Now().Truncate(time.Second)
	run := svc.Submit(moil.Job{
		Title: "Weekly sync",
		Params: probe(
			do.Reveal(),
			do.Copy("audio.ogg", "transcript.txt"),
			do.Result(json.RawMessage(meta), "transcript.txt"),
		),
		Inputs: map[string]moil.Download{"audio.ogg": svc.Serve(audio)},
		// The script doesn't make the subtitles, so they're neither
		// uploaded nor given to the hook.
		Outputs: map[string]moil.Upload{"transcript.txt": transcript.Upload, "subtitles.vtt": svc.Receive().Upload},
	})
	var seen struct {
		Context struct {
			Params json.RawMessage `json:"params"`
		} `json:"context"`
	}
	awaitEvent(t, run, "the script to reveal its params", func(ev moil.Event) bool {
		return ev.Kind == moil.EventData && json.Unmarshal(ev.Data, &seen) == nil && seen.Context.Params != nil
	})

	// The hook runs, and waits for the test; meanwhile the service hears
	// that the job succeeded.
	ran := hook.awaitRuns(1)[0]
	succeed(t, run)
	if !alive(ran.pid) {
		t.Fatal("the hook ended before the test let it")
	}

	c := ran.context
	home := m.realHome()
	if c.V != 1 {
		t.Errorf("the hook context's v is %d, want 1", c.V)
	}
	if want := (serviceRefJSON{ID: m.serviceID, Name: "e2e", BaseURL: svc.link.URL() + "/moil"}); c.Service != want {
		t.Errorf("the hook was told of the service %+v, want %+v", c.Service, want)
	}
	if want := (bundleRefJSON{Name: "probe", Version: "1", Hash: svc.probe.Hash()}); c.Bundle != want {
		t.Errorf("the hook was told of the bundle %+v, want %+v", c.Bundle, want)
	}
	if c.JobID != run.ID() || c.Attempt != 1 || c.Title != "Weekly sync" {
		t.Errorf("the hook was told of %s attempt %d, %q; want %s attempt 1, \"Weekly sync\"", c.JobID, c.Attempt, c.Title, run.ID())
	}
	if !sameJSON(c.Params, seen.Context.Params) {
		t.Errorf("the hook got the params %s, but the script got %s", c.Params, seen.Context.Params)
	}
	if !sameJSON(c.Meta, []byte(meta)) {
		t.Errorf("the hook got the meta %s, want the result's %s", c.Meta, meta)
	}
	if c.EndedAt.Before(submitted) || c.EndedAt.After(time.Now()) {
		t.Errorf("the hook was told the attempt ended at %v, before it was submitted or after now", c.EndedAt)
	}
	// The outputs' names are the service's, so the hook doesn't run
	// among them.
	if ran.cwd != ran.tmpdirPhysical || len(ran.cwdListing) != 0 {
		t.Errorf("the hook ran in %s, which held %v, not in its empty TMPDIR %s", ran.cwd, ran.cwdListing, ran.tmpdirPhysical)
	}
	if !filepath.IsAbs(c.OutputDir) || strings.HasPrefix(ran.tmpdir, c.OutputDir) {
		t.Errorf("the output directory %s isn't a directory of its own", c.OutputDir)
	}
	if want := map[string]string{"transcript.txt": filepath.Join(c.OutputDir, "transcript.txt")}; !maps.Equal(c.Outputs, want) {
		t.Errorf("the hook was told of the outputs %v, want %v", c.Outputs, want)
	}
	if !slices.Equal(ran.listing, []string{"transcript.txt"}) {
		t.Errorf("the output directory holds %v, not the outputs and nothing else", ran.listing)
	}
	uploaded, _ := transcript.Received()
	if got := ran.output("transcript.txt"); !slices.Equal(got, uploaded.body) || !slices.Equal(got, audio) {
		t.Errorf("the hook got %d bytes of transcript, the service %d, the script made %d", len(got), len(uploaded.body), len(audio))
	}

	// The environment moil gives it, but for what sh sets itself.
	env := maps.Clone(ran.env)
	for _, name := range []string{"PWD", "SHLVL", "_"} {
		delete(env, name)
	}
	wantEnv := map[string]string{
		"HOME":      os.Getenv("HOME"),
		"USER":      os.Getenv("USER"),
		"LANG":      "C.UTF-8",
		"MOIL_HOME": home,
		"TMPDIR":    ran.tmpdir,
		"PATH":      home + "/.machine/bin:/usr/bin:/bin:/usr/sbin:/sbin",
	}
	if !maps.Equal(env, wantEnv) {
		t.Errorf("the hook's environment is\n  %v\nwant\n  %v", env, wantEnv)
	}
	if !filepath.IsAbs(ran.tmpdir) || strings.HasPrefix(ran.tmpdir, c.OutputDir) || len(ran.tmpListing) != 0 {
		t.Errorf("the hook's TMPDIR %s isn't a fresh directory of its own: it held %v", ran.tmpdir, ran.tmpListing)
	}
	// .machine/bin on its PATH has this machine's moil.
	if ran.moilHome != (homeJSON{Home: home}) {
		t.Errorf("`moil home --json` in the hook says %+v, want %s", ran.moilHome, home)
	}

	finish.Open(t)
	log := hook.awaitLog("== exited")
	header := regexp.MustCompile(`^== (\S+) (\S+) attempt (\d+)$`).FindStringSubmatch(log[0])
	if len(log) != 4 || header == nil {
		t.Fatalf("the hook's log is\n%s", strings.Join(log, "\n"))
	}
	started, err := time.Parse(time.RFC3339, header[1])
	if err != nil || !strings.HasSuffix(header[1], "Z") || started.Before(c.EndedAt.Truncate(time.Second)) ||
		header[2] != run.ID() || header[3] != "1" {
		t.Errorf("the hook's log starts with %q, not with when it started, after %v, and %s attempt 1", log[0], c.EndedAt, run.ID())
	}
	if want := []string{ran.said, ran.complained}; !slices.Equal(log[1:3], want) {
		t.Errorf("the hook's log has %q between moil's lines, want what it printed, %q", log[1:3], want)
	}
	if !regexp.MustCompile(`^== exited 0 after \d+s$`).MatchString(log[3]) {
		t.Errorf("the hook's log ends with %q", log[3])
	}
	// What moil made for the hook is gone.
	for _, dir := range []string{c.OutputDir, ran.tmpdir} {
		eventually(t, dir+" to be removed", func() bool {
			_, err := os.Stat(dir)
			return errors.Is(err, fs.ErrNotExist)
		})
	}
	// The service heard nothing of the hook.
	for _, s := range []string{ran.said, ran.complained, hook.path, hook.saved} {
		if m.Sent(s) {
			t.Errorf("the machine sent the service %q", s)
		}
	}
}

// Hooks run one at a time, in the order their attempts ended, and only
// for attempts that succeeded. The machine takes jobs while a hook runs,
// and hooks run while the machine is paused. `moil logs -f` shows a
// hook's log as the hook writes it.
func TestHooksRunOneAtATimeInTheOrderTheirJobsEnded(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	proceed := newGate(t)
	hook := m.InstallHook("probe", proceed)

	first := svc.Submit(moil.Job{Title: "first", Params: probe(do.Result(nil))})
	succeed(t, first)
	running := hook.awaitRuns(1)[0]
	// The owner names the service as it names itself.
	follow := m.follow("logs", "e2e", "probe", "-f")
	if shown := follow.await(running.complained); !strings.HasPrefix(shown[0], "== ") || !strings.HasSuffix(shown[0], first.ID()+" attempt 1") {
		t.Errorf("moil logs -f showed %q", shown)
	}

	// The machine takes jobs while the hook waits. A failed one has no
	// hook to run.
	failed := svc.Submit(moil.Job{Title: "failed", Params: probe(do.Fail("the recording is empty", false))})
	failure(t, failed)
	status := m.awaitStatus("the failed job as the last", func(s statusJSON) bool {
		return s.LastJob != nil && s.LastJob.JobID == failed.ID()
	})
	if last := status.LastJob; last.Outcome != "failed" || last.Error == nil ||
		*last.Error != (errorJSON{Code: string(moil.CodeScriptError), Message: "the recording is empty"}) {
		t.Errorf("moil status --json shows the failed job as %+v, with the error %+v", last, last.Error)
	}
	second := svc.Submit(moil.Job{Title: "second", Params: probe(do.Result(nil))})
	succeed(t, second)

	var paused pausedJSON
	if m.json(&paused, "pause"); !paused.Paused {
		t.Error("moil pause --json says the machine isn't paused")
	}
	eventually(t, "the service to see the machine paused", func() bool { return svc.Machine(m.ID()).State == moil.Paused })
	proceed.Open(t)
	runs := hook.awaitEnded(2)

	// The second hook started once the first had ended, and no hook ran
	// for the failed job, which ended between them.
	want := []string{"started " + runs[0].name, "ended " + runs[0].name, "started " + runs[1].name, "ended " + runs[1].name}
	if got := hook.events(); !slices.Equal(got, want) {
		t.Errorf("the hook's runs went %q, want %q", got, want)
	}
	if runs[0].context.JobID != first.ID() || runs[1].context.JobID != second.ID() {
		t.Errorf("the hooks ran for %s then %s, want %s (first) then %s (second), and none for %s",
			runs[0].context.JobID, runs[1].context.JobID, first.ID(), second.ID(), failed.ID())
	}
	if runs[0].tmpdir == runs[1].tmpdir {
		t.Errorf("the hooks shared the TMPDIR %s", runs[0].tmpdir)
	}
	follow.await("== exited 0")
	shown := follow.await("== exited 0")
	if !strings.HasSuffix(shown[0], second.ID()+" attempt 1") || shown[1] != runs[1].said {
		t.Errorf("moil logs -f showed the second hook's run as %q", shown)
	}
	if status := m.Status(); !status.Paused {
		t.Error("the machine didn't stay paused")
	}
}

// moil runs a hook's file directly, so the file must say what runs it:
// with a #! line, such as one that has uv run it as a Python script, or by
// being a program. moil doesn't guess at a shell for a file that is
// neither: it says why in the hook's log, `moil hooks` and `moil status`.
func TestMoilRunsOnlyHooksThatSayWhatRunsThem(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	ran := filepath.Join(t.TempDir(), "ran")
	path := m.hookPath("probe")
	writeFile(t, path, "touch "+shellQuote(ran)+"\n", 0o755)

	var hooks []hookListingJSON
	m.json(&hooks, "hooks")
	want := hookListingJSON{
		Service:  m.serviceID,
		Bundle:   "probe",
		Path:     path,
		Log:      filepath.Join(m.serviceDir(), "logs", "probe.log"),
		Runnable: false,
	}
	if len(hooks) != 1 || hooks[0] != want {
		t.Errorf("moil hooks --json lists %+v, want %+v", hooks, want)
	}
	where := "services/" + m.serviceID + "/hooks/probe"
	status := m.awaitStatus("the hook's problem", func(status statusJSON) bool {
		return slices.ContainsFunc(status.Problems, func(p problemJSON) bool { return p.Path == where })
	})

	run := svc.Submit(moil.Job{Params: probe(do.Result(nil))})
	succeed(t, run)
	log := awaitHookLog(t, m, "probe", "== not run")
	why, _ := strings.CutPrefix(log[len(log)-1], "== not run: ")
	if len(log) != 2 || !strings.HasSuffix(log[0], run.ID()+" attempt 1") || !strings.Contains(why, "#!") {
		t.Errorf("the hook's log is\n%s", strings.Join(log, "\n"))
	}
	if problem := (problemJSON{Path: where, Message: "moil can't run this hook: " + why}); !slices.Contains(status.Problems, problem) {
		t.Errorf("moil status --json reports %+v, not the hook's log's reason, %q", status.Problems, why)
	}
	if _, err := os.Stat(ran); !errors.Is(err, fs.ErrNotExist) {
		t.Error("moil ran the hook, which says nothing of what runs it")
	}

	// With a #! line it runs: uv, from .machine/bin on its PATH, runs it
	// as a Python script. The owner keeps it elsewhere, and links it.
	kept := filepath.Join(t.TempDir(), "dotfiles", "keep.py")
	writeFile(t, kept, strings.Join([]string{
		"#!/usr/bin/env -S uv run --script",
		"# /// script",
		`# requires-python = ">=3.12"`,
		"# dependencies = []",
		"# ///",
		"import json, sys",
		"context = json.load(sys.stdin)",
		"open(" + strconv.Quote(ran) + `, "w").write(context["job_id"])`,
		`print("kept", context["job_id"])`,
		"",
	}, "\n"), 0o755)
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(kept, path); err != nil {
		t.Fatal(err)
	}
	m.awaitStatus("no problem", func(status statusJSON) bool { return len(status.Problems) == 0 })
	next := svc.Submit(moil.Job{Params: probe(do.Result(nil))})
	succeed(t, next)
	log = awaitHookLog(t, m, "probe", "== exited")
	if last := log[len(log)-1]; !strings.HasPrefix(last, "== exited 0 after ") {
		t.Errorf("the Python hook's log is\n%s", strings.Join(log, "\n"))
	}
	if kept, err := os.ReadFile(ran); err != nil || string(kept) != next.ID() {
		t.Errorf("the Python hook kept %q, %v; want %s", kept, err, next.ID())
	}
}

// moil records how a hook ended, and doesn't run a hook that failed
// again, even once it restarts.
func TestAFailedHookIsntRunAgain(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	// The first run fails, and the second kills itself.
	runs := filepath.Join(t.TempDir(), "runs")
	writeFile(t, m.hookPath("probe"), fmt.Sprintf(`#!/bin/sh
echo $$ >> %s
case $(wc -l < %s) in
	*1) echo "failing"; exit 3 ;;
	*2) echo "dying"; kill -KILL $$ ;;
esac
`, shellQuote(runs), shellQuote(runs)), 0o755)

	first := svc.Submit(moil.Job{Params: probe(do.Result(nil))})
	succeed(t, first)
	awaitHookLog(t, m, "probe", "== exited 3 after ")
	m.Stop()
	m.Start()
	second := svc.Submit(moil.Job{Params: probe(do.Result(nil))})
	succeed(t, second)
	log := awaitHookLog(t, m, "probe", "== killed by signal 9 after ")
	want := []string{"== ", "failing", "== exited 3 after ", "== ", "dying", "== killed by signal 9 after "}
	if len(log) != len(want) {
		t.Fatalf("the hook's log is\n%s", strings.Join(log, "\n"))
	}
	for i, line := range log {
		if !strings.HasPrefix(line, want[i]) {
			t.Errorf("line %d of the hook's log is %q, want %q…", i+1, line, want[i])
		}
	}
	if !strings.HasSuffix(log[3], second.ID()+" attempt 1") {
		t.Errorf("the hook's second run was %q, not for %s", log[3], second.ID())
	}
}

// When moil stops while a hook runs, the hook runs again from the start,
// with the same context and outputs, when moil next starts. After a
// crash, moil first kills what the hook left, a child that ignores
// SIGTERM included, and closes its log with a line saying so; `moil quit`
// stops the hook as moil quits.
func TestAHookSurvivesItsMachineCrashing(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	proceed := newGate(t)
	hook := m.InstallHook("probe", proceed, leaveAChild)
	transcript := randomBytes(t, 64<<10)
	run := svc.Submit(moil.Job{
		Params:  probe(do.Copy("audio.ogg", "transcript.txt"), do.Result(nil, "transcript.txt")),
		Inputs:  map[string]moil.Download{"audio.ogg": svc.Serve(transcript)},
		Outputs: map[string]moil.Upload{"transcript.txt": svc.Receive().Upload},
	})
	succeed(t, run)
	first := hook.awaitRuns(1)[0]

	m.Kill()
	if !alive(first.pid) || !alive(first.child) {
		t.Fatal("the hook ended with moil; the test can't tell whether moil cleans up after it")
	}
	m.Start()
	awaitGone(t, "the hook the crash left behind", first.pid, first.child)
	second := hook.awaitRuns(2)[1]

	var quit nothingJSON
	m.json(&quit, "quit")
	if err := m.Exited(); err != nil {
		t.Errorf("moil agent exited with %v after moil quit", err)
	}
	awaitGone(t, "the hook moil quit", second.pid, second.child)
	m.Start()
	third := hook.awaitRuns(3)[2]
	proceed.Open(t)
	hook.awaitEnded(1)

	for _, again := range []hookRun{second, third} {
		if !sameJSON(again.raw, first.raw) {
			t.Errorf("the hook ran again with the context\n  %s\nnot\n  %s", again.raw, first.raw)
		}
		if !slices.Equal(again.output("transcript.txt"), transcript) {
			t.Error("the hook ran again without the attempt's output")
		}
		if len(again.tmpListing) != 0 {
			t.Errorf("the hook ran again with a TMPDIR holding %v from its last run", again.tmpListing)
		}
	}
	header := "attempt 1"
	log := hook.awaitLog("== exited")
	want := []string{
		header, first.said, first.complained, "== stopped: moil stopped unexpectedly",
		header, second.said, second.complained, "== stopped: moil quit",
		header, third.said, third.complained, "== exited 0 after ",
	}
	if len(log) != len(want) {
		t.Fatalf("the hook's log is\n%s", strings.Join(log, "\n"))
	}
	for i, line := range log {
		if want[i] == header {
			if !strings.HasPrefix(line, "== ") || !strings.HasSuffix(line, run.ID()+" attempt 1") {
				t.Errorf("line %d of the hook's log is %q, not a run's header", i+1, line)
			}
		} else if !strings.HasPrefix(line, want[i]) {
			t.Errorf("line %d of the hook's log is %q, want %q", i+1, line, want[i])
		}
	}
}

// A shellHook is the owner's hook for a bundle as the tests write it: a
// shell script that saves what moil gives it where the test can read it,
// prints a line to stdout and one to stderr, and then, if it has a gate,
// waits until the gate is open before it exits.
type shellHook struct {
	t      *testing.T
	m      *machine
	bundle string
	// path is where the hook is installed.
	path string
	// saved holds a folder for each run, named by its pid, with what it
	// got, and a file listing when each run started and ended.
	saved string
}

// leaveAChild makes a shellHook start a child that ignores SIGTERM, and
// save its pid, before it waits.
const leaveAChild = `(trap '' TERM; exec /bin/sleep 60) &
echo $! > "$run/child"`

// InstallHook installs a shellHook for bundle, for the service the
// machine is paired with, with extra lines to run before it waits. It
// records its process group where the service's leak check finds it.
func (m *machine) InstallHook(bundle string, proceed gate, extra ...string) *shellHook {
	m.t.Helper()
	h := &shellHook{t: m.t, m: m, bundle: bundle, path: m.hookPath(bundle), saved: m.t.TempDir()}
	script := fmt.Sprintf(`#!/bin/sh
# The end-to-end tests' hook: it saves what moil gives it in $saved, then
# waits for $gate, giving up after a minute.
set -e
saved=%s
record=%s
gate=%s
echo $$ > "$record/hook.$$"
echo "started $$" >> "$saved/runs"
run="$saved/$$"
mkdir "$run"
cat > "$run/context.json"
out=$(sed -n 's/.*"output_dir":"\([^"]*\)".*/\1/p' "$run/context.json")
pwd -P > "$run/cwd"
ls -A > "$run/cwd-listing"
/usr/bin/env > "$run/env"
ls -A "$out" > "$run/listing"
mkdir "$run/outputs"
cp -R "$out/" "$run/outputs/"
ls -A "$TMPDIR" > "$run/tmp-listing"
(cd "$TMPDIR" && pwd -P) > "$run/tmpdir-physical"
touch "$TMPDIR/scratch"
echo "$TMPDIR" > "$run/tmpdir"
moil home --json > "$run/moil-home"
%s
touch "$run/saved"
echo "hook $$ saved what moil gave it"
echo "hook $$ complains on stderr" >&2
waited=0
while [ -n "$gate" ] && [ ! -e "$gate" ]; do
	waited=$((waited + 1))
	if [ $waited -gt 1200 ]; then
		echo "hook $$ gave up waiting for $gate" >&2
		exit 1
	fi
	sleep 0.05
done
echo "ended $$" >> "$saved/runs"
`, shellQuote(h.saved), shellQuote(m.svc.record), shellQuote(string(proceed)), strings.Join(extra, "\n"))
	writeFile(m.t, h.path, script, 0o755)
	return h
}

// hookPath is where the owner's hook for bundle goes.
func (m *machine) hookPath(bundle string) string {
	return filepath.Join(m.serviceDir(), "hooks", bundle)
}

// A hookRun is what one run of a shellHook got.
type hookRun struct {
	pid  int
	name string // its folder's name: the pid
	dir  string
	// raw is what the hook read on stdin; context is raw decoded.
	raw        []byte
	context    hookContextJSON
	cwd        string
	cwdListing []string // what its working directory held as it started
	env        map[string]string
	listing    []string // what its output directory held
	tmpdir     string
	// tmpdirPhysical is tmpdir with symbolic links resolved, as pwd -P
	// shows the working directory.
	tmpdirPhysical string
	tmpListing     []string // what its TMPDIR held as it started
	moilHome       homeJSON // `moil home --json`, run by the hook
	child          int      // the child it left, with leaveAChild
	// said and complained are what it printed to stdout and stderr.
	said, complained string
}

// output returns an output as the hook got it.
func (r hookRun) output(name string) []byte {
	data, _ := os.ReadFile(filepath.Join(r.dir, "outputs", name))
	return data
}

// events returns when each run of the hook started and ended, in order:
// "started <pid>" and "ended <pid>".
func (h *shellHook) events() []string {
	data, _ := os.ReadFile(filepath.Join(h.saved, "runs"))
	return strings.FieldsFunc(string(data), func(r rune) bool { return r == '\n' })
}

// awaitRuns waits until n runs of the hook have saved what they got, and
// returns what they did, in the order they started.
func (h *shellHook) awaitRuns(n int) []hookRun {
	h.t.Helper()
	var started []string
	eventually(h.t, fmt.Sprintf("the %s hook to run %d times", h.bundle, n), func() bool {
		started = h.started()
		if len(started) < n {
			return false
		}
		for _, name := range started {
			if _, err := os.Stat(filepath.Join(h.saved, name, "saved")); err != nil {
				return false
			}
		}
		return true
	})
	runs := make([]hookRun, len(started))
	for i, name := range started {
		runs[i] = h.load(name)
	}
	return runs
}

// awaitEnded waits until n runs of the hook have got to its end, and
// returns them in the order they started.
func (h *shellHook) awaitEnded(n int) []hookRun {
	h.t.Helper()
	var ended []string
	eventually(h.t, fmt.Sprintf("the %s hook to end %d times", h.bundle, n), func() bool {
		ended = nil
		for _, event := range h.events() {
			if name, ok := strings.CutPrefix(event, "ended "); ok {
				ended = append(ended, name)
			}
		}
		return len(ended) >= n
	})
	runs := make([]hookRun, len(ended))
	for i, name := range ended {
		runs[i] = h.load(name)
	}
	return runs
}

// started returns the pids of the runs that started, in order.
func (h *shellHook) started() []string {
	var started []string
	for _, event := range h.events() {
		if name, ok := strings.CutPrefix(event, "started "); ok {
			started = append(started, name)
		}
	}
	return started
}

func (h *shellHook) load(name string) hookRun {
	h.t.Helper()
	dir := filepath.Join(h.saved, name)
	read := func(file string) string {
		data, err := os.ReadFile(filepath.Join(dir, file))
		if err != nil {
			h.t.Fatalf("the hook didn't save %s: %v", file, err)
		}
		return string(data)
	}
	r := hookRun{name: name, dir: dir, raw: []byte(read("context.json"))}
	r.pid, _ = strconv.Atoi(name)
	r.said = "hook " + name + " saved what moil gave it"
	r.complained = "hook " + name + " complains on stderr"
	if strings.Count(string(r.raw), "\n") != 1 || !strings.HasSuffix(string(r.raw), "\n") {
		h.t.Errorf("the hook read %q on stdin, not one line of JSON", r.raw)
	}
	if err := decodeStrict(r.raw, &r.context); err != nil {
		h.t.Fatalf("the hook context isn't in the spec's shape: %v\n%s", err, r.raw)
	}
	r.cwd = strings.TrimSpace(read("cwd"))
	r.cwdListing = strings.Fields(read("cwd-listing"))
	r.env = map[string]string{}
	for _, line := range strings.Split(strings.TrimSpace(read("env")), "\n") {
		name, value, _ := strings.Cut(line, "=")
		r.env[name] = value
	}
	r.listing = strings.Fields(read("listing"))
	r.tmpdir = strings.TrimSpace(read("tmpdir"))
	r.tmpdirPhysical = strings.TrimSpace(read("tmpdir-physical"))
	r.tmpListing = strings.Fields(read("tmp-listing"))
	if err := decodeStrict([]byte(read("moil-home")), &r.moilHome); err != nil {
		h.t.Errorf("`moil home --json` printed JSON that isn't in the spec's shape: %v", err)
	}
	if child, err := os.ReadFile(filepath.Join(dir, "child")); err == nil {
		r.child, _ = strconv.Atoi(strings.TrimSpace(string(child)))
	}
	return r
}

// awaitLog waits until the hook's log has a line that starts with
// prefix, and returns its lines.
func (h *shellHook) awaitLog(prefix string) []string {
	h.t.Helper()
	return awaitHookLog(h.t, h.m, h.bundle, prefix)
}

// awaitHookLog waits until the log of m's hook for bundle has a line that
// starts with prefix, and returns its lines.
func awaitHookLog(t *testing.T, m *machine, bundle, prefix string) []string {
	t.Helper()
	path := filepath.Join(m.serviceDir(), "logs", bundle+".log")
	var lines []string
	eventually(t, "the "+bundle+" hook's log to say "+prefix, func() bool {
		data, _ := os.ReadFile(path)
		lines = strings.Split(strings.TrimSuffix(string(data), "\n"), "\n")
		return slices.ContainsFunc(lines, func(line string) bool { return strings.HasPrefix(line, prefix) })
	})
	return lines
}

// writeFile writes a file, making its folder if need be.
func writeFile(t *testing.T, path, text string, mode os.FileMode) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(text), mode); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, mode); err != nil {
		t.Fatal(err)
	}
}
