package e2e

import (
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// An owner's agent, such as an LLM with a shell, drives moil through its
// command line alone, reading only JSON. While `moil agent` runs, it
// reads the status, approves a bundle, pauses and resumes the machine,
// stops the running job, revokes an approval, unpairs and pairs again,
// and quits the agent; each change reaches the service at once. Every
// command prints JSON in the spec's shapes, and those that need a running
// agent exit with status 3 once none runs.
func TestAnOwnersAgentDrivesMoilThroughItsCommandLine(t *testing.T) {
	t.Parallel()
	svc := newService(t)
	echo := svc.publishEcho()
	m := newMachine(t, "alice-mac")
	baseURL := svc.link.URL() + "/moil"
	m.PairJSON(svc, "alice", baseURL)
	var approved approvedJSON
	m.json(&approved, "approve", m.serviceID, svc.probe.Hash(), "--yes")
	if approved.Approved != svc.probe.Hash() {
		t.Fatalf("moil approve --json says it approved %q, want %s", approved.Approved, svc.probe.Hash())
	}
	// A hook the owner hasn't made executable yet.
	writeFile(t, m.hookPath("transcribe"), "#!/bin/sh\n", 0o644)
	m.Start()
	watch := m.follow("status", "--watch", "--json")
	var stopping stoppingJSON
	if m.json(&stopping, "stop"); stopping.Stopping != "" {
		t.Errorf("moil stop --json says it's stopping %q, with no job running", stopping.Stopping)
	}

	var home homeJSON
	if m.json(&home, "home"); home.Home != m.home {
		t.Errorf("moil home --json says %q, want %s", home.Home, m.home)
	}
	version := strings.Fields(m.moil("--version"))
	status := m.awaitStatus("the service's bundles", func(s statusJSON) bool {
		return len(s.Services) == 1 && len(s.Services[0].Offered) == 2
	})
	if status.Home != m.home || status.Agent == nil || *status.Agent != (agentJSON{Host: "agent", Version: version[len(version)-1]}) ||
		status.Paused || status.Job != nil || status.LastJob != nil {
		t.Errorf("moil status --json says %+v, not that moil agent %s runs on %s, idle", status, version, m.home)
	}
	service := status.Services[0]
	hook := hookJSON{
		Bundle:   "transcribe",
		Path:     m.hookPath("transcribe"),
		Log:      filepath.Join(m.serviceDir(), "logs", "transcribe.log"),
		Runnable: false,
	}
	if service.ID != m.serviceID || service.Name != "e2e" || service.BaseURL != baseURL || !service.Paired ||
		service.Connection == nil || *service.Connection != (connectionJSON{State: "connected"}) ||
		!slices.Equal(service.Hooks, []hookJSON{hook}) {
		t.Errorf("moil status --json shows the service as %+v", service)
	}
	if a := service.Approvals; len(a) != 1 || a[0].Hash != svc.probe.Hash() || a[0].Name != "probe" || a[0].Version != "1" ||
		a[0].ApprovedAt.After(time.Now()) {
		t.Errorf("moil status --json shows the approvals %+v, want the probe's", a)
	}
	offered := map[string]offeredJSON{}
	for _, b := range service.Offered {
		offered[b.Name] = b
	}
	wantOffered := map[string]offeredJSON{
		"probe": {Hash: svc.probe.Hash(), Name: "probe", Version: "1", Description: svc.probe.Description(), Approved: true},
		"echo":  {Hash: echo.Hash(), Name: "echo", Version: "1.0.0", Description: echo.Description(), Approved: false},
	}
	if !maps.Equal(offered, wantOffered) {
		t.Errorf("moil status --json shows the offered bundles %+v, want %+v", offered, wantOffered)
	}
	problem := problemJSON{
		Path:    "services/" + m.serviceID + "/hooks/transcribe",
		Message: "moil can't run this hook: it isn't executable. Run chmod +x on it.",
	}
	if !slices.Equal(status.Problems, []problemJSON{problem}) {
		t.Errorf("moil status --json reports the problems %+v, want %+v", status.Problems, problem)
	}
	var services []serviceJSON
	if m.json(&services, "services"); !reflect.DeepEqual(services, status.Services) {
		t.Errorf("moil services --json says\n  %+v\nbut moil status --json\n  %+v", services, status.Services)
	}
	var hooks []hookListingJSON
	listing := hookListingJSON{Service: m.serviceID, Bundle: hook.Bundle, Path: hook.Path, Log: hook.Log, Runnable: hook.Runnable}
	if m.json(&hooks, "hooks"); !slices.Equal(hooks, []hookListingJSON{listing}) {
		t.Errorf("moil hooks --json says %+v, want %+v", hooks, listing)
	}

	// It sees the machine lose the service, and find it again.
	svc.link.Cut()
	status = m.awaitStatus("the service offline", func(s statusJSON) bool {
		return s.Services[0].Connection != nil && s.Services[0].Connection.State == "offline"
	})
	if c := status.Services[0].Connection; c.Error == "" || c.RetryAt == nil || c.RetryAt.Before(time.Now().Add(-time.Minute)) {
		t.Errorf("moil status --json shows the lost connection as %+v", c)
	}
	svc.link.Restore()
	m.awaitStatus("the service connected again", func(s statusJSON) bool {
		return *s.Services[0].Connection == connectionJSON{State: "connected"}
	})

	// The owner's agent reads the bundle waiting for review.
	var bundles []listedJSON
	m.json(&bundles, "review", m.serviceID)
	listed := map[string]listedJSON{}
	for _, b := range bundles {
		listed[b.Name] = b
	}
	wantListed := map[string]listedJSON{
		"probe": {Hash: svc.probe.Hash(), Name: "probe", Version: "1", Description: svc.probe.Description(), Approved: true, Offered: true},
		"echo":  {Hash: echo.Hash(), Name: "echo", Version: "1.0.0", Description: echo.Description(), Approved: false, Offered: true},
	}
	if !maps.Equal(listed, wantListed) {
		t.Errorf("moil review --json lists %+v, want %+v", listed, wantListed)
	}
	var review reviewJSON
	m.json(&review, "review", m.serviceID, echo.Hash())
	wantFiles := map[string]string{}
	for _, name := range []string{moil.ScriptFile, moil.LockFile, moil.ManifestFile} {
		wantFiles[name] = string(echo.File(name))
	}
	if review.Service != (serviceRefJSON{ID: m.serviceID, Name: "e2e", BaseURL: baseURL}) || review.Hash != echo.Hash() ||
		review.Approved || !maps.Equal(review.Files, wantFiles) || review.Packages == nil || review.Previous != nil {
		t.Errorf("moil review --json shows the echo bundle as %+v", review)
	}

	// It approves the bundle while the agent runs, naming the service by
	// its URL, and a job waiting for it runs. Without --yes, moil won't
	// approve it, since nobody answers at a terminal.
	waiting := svc.Submit(moil.Job{Bundle: echo, Params: map[string]any{"text": "hi"}})
	if stdout, _, err := m.try("approve", baseURL, echo.Hash(), "--json"); err == nil || stdout != "" {
		t.Errorf("moil approve --json without --yes approved (%v), printing %q", err, stdout)
	}
	if stdout, stderr, err := m.try("approve", baseURL, echo.Hash()); err == nil || !strings.Contains(stdout, echo.Hash()) ||
		!strings.Contains(stderr, "--yes") {
		t.Errorf("moil approve without --yes or a terminal: %v, printing\n%s\n%s", err, stdout, stderr)
	}
	m.json(&approved, "approve", baseURL, echo.Hash(), "--yes")
	if approved.Approved != echo.Hash() {
		t.Errorf("moil approve --json says it approved %q, want %s", approved.Approved, echo.Hash())
	}
	if res := succeed(t, waiting); res.Machine != m.ID() {
		t.Errorf("the echo job ran on %s, not %s", res.Machine, m.ID())
	}

	// It pauses the machine, which the service sees, and resumes it.
	var paused pausedJSON
	if m.json(&paused, "pause"); !paused.Paused {
		t.Error("moil pause --json says the machine isn't paused")
	}
	eventually(t, "the service to see the machine paused", func() bool { return svc.Machine(m.ID()).State == moil.Paused })
	// The agent saves the choice in settings.json a moment later.
	eventually(t, "settings.json to say the machine is paused", func() bool { return readSettings(t, m)["paused"] == true })
	stopped, revoked := newGate(t), newGate(t)
	title := "Weekly sync \u202e"
	submitted := time.Now().Truncate(time.Second)
	job := svc.Submit(moil.Job{Title: title, Params: probeAttempts(
		[]step{do.Progress(0.42, "diarizing"), do.WaitFor(stopped)},
		[]step{do.WaitFor(revoked)},
	)})
	if m.json(&paused, "resume"); paused.Paused {
		t.Error("moil resume --json says the machine is paused")
	}

	// It reads the running job's progress.
	status = m.awaitStatus("the job's progress", func(s statusJSON) bool {
		return s.Job != nil && s.Job.Fraction != nil
	})
	running := status.Job
	if running.Service != m.serviceID || running.JobID != job.ID() || running.Attempt != 1 || running.Title != title ||
		running.Bundle != "probe" || running.Phase != "running" || *running.Fraction != 0.42 || running.Message != "diarizing" ||
		running.StartedAt.Before(submitted) || running.StartedAt.After(time.Now()) || running.Stopping {
		t.Errorf("moil status --json shows the running job as %+v", running)
	}
	if raw := m.moil("status", "--json"); !strings.Contains(raw, `"title":"Weekly sync \u202e"`) {
		t.Errorf("moil status --json doesn't escape the hidden character in the job's title:\n%s", raw)
	}

	// It stops the job, which the service sees interrupted and retries.
	if m.json(&stopping, "stop"); stopping.Stopping != job.ID()+" attempt 1" {
		t.Errorf("moil stop --json says it's stopping %q, want %s attempt 1", stopping.Stopping, job.ID())
	}
	if e := awaitRetry(t, job, 1); e.Code != moil.CodeInterrupted || !e.Retryable {
		t.Errorf("the stopped attempt failed with %+v, want a retryable %s", e, moil.CodeInterrupted)
	}
	status = m.awaitStatus("the stopped attempt as the last job", func(s statusJSON) bool {
		return s.LastJob != nil && s.Job != nil && s.Job.Attempt == 2
	})
	if last := status.LastJob; last.Service != m.serviceID || last.JobID != job.ID() || last.Attempt != 1 || last.Bundle != "probe" ||
		last.Outcome != "interrupted" || last.Error != nil || last.EndedAt.Before(running.StartedAt.Time) {
		t.Errorf("moil status --json shows the last job as %+v", last)
	}

	// It revokes the probe, naming the service by its name, which stops
	// the probe's running attempt.
	var revokedHash revokedJSON
	if m.json(&revokedHash, "revoke", "e2e", svc.probe.Hash()); revokedHash.Revoked != svc.probe.Hash() {
		t.Errorf("moil revoke --json says it revoked %q, want %s", revokedHash.Revoked, svc.probe.Hash())
	}
	if e := awaitRetry(t, job, 2); e.Code != moil.CodeInterrupted {
		t.Errorf("the revoked bundle's attempt failed with %+v, want %s", e, moil.CodeInterrupted)
	}
	eventually(t, "the service to learn of the revoked approval", func() bool {
		return !svc.Machine(m.ID()).HasApproved(svc.probe.Hash())
	})
	if _, err := os.Stat(filepath.Join(m.serviceDir(), "bundles", "probe")); !errors.Is(err, fs.ErrNotExist) {
		t.Errorf("the revoked bundle's files are still in the service's folder: %v", err)
	}

	// It unpairs: the service forgets the machine, and the machine its
	// pairing, but the folder stays with its approvals and hooks.
	var unpaired unpairedJSON
	if m.json(&unpaired, "unpair", m.serviceID); unpaired.Unpaired != m.serviceID {
		t.Errorf("moil unpair --json says it unpaired %q, want %s", unpaired.Unpaired, m.serviceID)
	}
	if _, err := svc.sdk.Machine(context.Background(), m.ID()); !errors.Is(err, moil.ErrNotFound) {
		t.Errorf("the service still knows the unpaired machine: %v", err)
	}
	if _, err := os.Stat(filepath.Join(m.serviceDir(), "pairing.json")); !errors.Is(err, fs.ErrNotExist) {
		t.Errorf("pairing.json is still there after moil unpair: %v", err)
	}
	for _, kept := range []string{"service.json", "bundles/echo/" + echo.Hash() + "/job.py", "hooks/transcribe"} {
		if _, err := os.Stat(filepath.Join(m.serviceDir(), kept)); err != nil {
			t.Errorf("moil unpair took %s from the service's folder: %v", kept, err)
		}
	}
	if tokens := readSecrets(t, m); len(tokens) != 0 {
		t.Errorf("the machine kept %d tokens after moil unpair", len(tokens))
	}
	m.json(&services, "services")
	if len(services) != 1 || services[0].Paired || services[0].Connection != nil || services[0].Offered != nil ||
		len(services[0].Approvals) != 1 || services[0].Approvals[0].Hash != echo.Hash() {
		t.Errorf("moil services --json shows the unpaired service as %+v", services)
	}

	// It pairs again, through the agent, which keeps the approvals.
	m.PairJSON(svc, "alice", m.serviceID)
	eventually(t, "the service to see the paired machine approve echo", func() bool {
		machine, err := svc.sdk.Machine(context.Background(), m.ID())
		return err == nil && machine.Online() && machine.HasApproved(echo.Hash()) && !machine.HasApproved(svc.probe.Hash())
	})
	// It reviews a new version of echo, with what changed since the one
	// approved.
	echo2 := publish(t, svc, echo, map[string]string{
		moil.ManifestFile: strings.Replace(string(echo.File(moil.ManifestFile)), `"1.0.0"`, `"1.1.0"`, 1),
		moil.ScriptFile:   string(echo.File(moil.ScriptFile)) + "\n# 1.1.0 changes nothing\n",
	})
	m.json(&review, "review", m.serviceID, echo2.Hash())
	if p := review.Previous; review.Approved || p == nil || p.Hash != echo.Hash() || p.Version != "1.0.0" ||
		!slices.Equal(slices.Sorted(maps.Keys(p.Changes)), []string{moil.ScriptFile, moil.ManifestFile}) ||
		!strings.Contains(p.Changes[moil.ScriptFile], "+# 1.1.0 changes nothing") {
		t.Errorf("moil review --json shows echo 1.1.0 as approved %v, replacing %+v", review.Approved, review.Previous)
	}

	// It quits the agent, which ends moil agent, and moil status --watch
	// with it.
	var quit nothingJSON
	m.json(&quit, "quit")
	if err := m.Exited(); err != nil {
		t.Errorf("moil agent exited with %v after moil quit", err)
	}
	if err := watch.exited(); err != nil {
		t.Errorf("moil status --watch --json exited with %v once the agent stopped", err)
	}
	watched := watch.rest()
	var sawPaused, sawProgress bool
	for i, line := range watched {
		var s statusJSON
		if err := decodeStrict([]byte(line), &s); err != nil {
			t.Fatalf("line %d of moil status --watch --json isn't a status in the spec's shape: %v\n%s", i+1, err, line)
		}
		sawPaused = sawPaused || s.Paused
		sawProgress = sawProgress || (s.Job != nil && s.Job.Fraction != nil && *s.Job.Fraction == 0.42)
	}
	if !sawPaused || !sawProgress {
		t.Errorf("moil status --watch --json printed %d statuses, none of them paused (%v) or with the job's progress (%v)",
			len(watched), sawPaused, sawProgress)
	}

	// With no agent, what needs one exits with status 3, and moil status
	// reads the home.
	for _, args := range [][]string{{"stop", "--json"}, {"quit", "--json"}, {"status", "--watch", "--json"}} {
		stdout, stderr, err := m.try(args...)
		var exit *exec.ExitError
		if !errors.As(err, &exit) || exit.ExitCode() != 3 || stdout != "" || !strings.Contains(stderr, "moil isn't running") {
			t.Errorf("moil %s with no agent: %v, printing %q and %q; want exit status 3", strings.Join(args, " "), err, stdout, stderr)
		}
	}
	if status := m.Status(); status.Agent != nil || status.Job != nil || status.LastJob != nil || status.Paused ||
		len(status.Services) != 1 || !status.Services[0].Paired || status.Services[0].Connection != nil || status.Services[0].Offered != nil {
		t.Errorf("moil status --json with no agent says %+v", status)
	}
}

// With settings.json invalid, `moil pause` fails without an agent: moil
// can't keep the choice. Through a running agent it must fail the same
// way (spec/machine.md §3.1), rather than report a pause that lasts only
// until the agent next starts.
func TestAPauseMoilCantKeepFailsWithAnAgentToo(t *testing.T) {
	t.Parallel()
	_, m := lentMachine(t)
	if err := os.WriteFile(filepath.Join(m.home, "settings.json"), []byte(`{"pasued": true}`+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	m.awaitStatus("settings.json as a problem", func(s statusJSON) bool {
		return slices.ContainsFunc(s.Problems, func(p problemJSON) bool { return p.Path == "settings.json" })
	})
	stdout, stderr, err := m.try("pause", "--json")
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 1 || stdout != "" || !strings.Contains(stderr, "settings.json") {
		t.Errorf("moil pause with settings.json invalid: %v, printing %q and %q; want exit status 1, and why", err, stdout, stderr)
	}
}

// publish publishes a new version of bundle, with files changed as
// given.
func publish(t *testing.T, svc *service, bundle *moil.Bundle, changed map[string]string) *moil.Bundle {
	t.Helper()
	files := map[string][]byte{}
	for _, name := range []string{moil.ScriptFile, moil.LockFile, moil.ManifestFile} {
		files[name] = bundle.File(name)
	}
	for name, text := range changed {
		files[name] = []byte(text)
	}
	next, err := moil.NewBundle(files)
	if err != nil {
		t.Fatal(err)
	}
	svc.sdk.AddBundle(next)
	return next
}

// readSettings reads the machine's settings.json.
func readSettings(t *testing.T, m *machine) map[string]any {
	t.Helper()
	data, err := os.ReadFile(filepath.Join(m.home, "settings.json"))
	if err != nil {
		t.Fatal(err)
	}
	var settings map[string]any
	if err := json.Unmarshal(data, &settings); err != nil {
		t.Fatalf("settings.json isn't JSON: %v", err)
	}
	return settings
}

// readSecrets reads the tokens the machine keeps, if any.
func readSecrets(t *testing.T, m *machine) map[string]string {
	t.Helper()
	data, err := os.ReadFile(filepath.Join(m.home, ".machine", "secrets.json"))
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	var secrets map[string]string
	if err != nil || json.Unmarshal(data, &secrets) != nil {
		t.Fatalf("reading the machine's tokens: %v, %s", err, data)
	}
	return secrets
}
