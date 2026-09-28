package e2e

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// A machine is a computer lent to a service with the moil command line:
// a moil home of its own, the moil binary and, while it runs, `moil
// agent`.
type machine struct {
	t    *testing.T
	name string
	home string
	log  *testLog
	// agentLog holds what every `moil agent` run wrote to stderr.
	agentLog lines

	// Set by pairing.
	svc       *service // the harness service, if paired with one
	id        string   // the service's ID for this machine
	serviceID string   // this machine's ID for the service

	agent *agentProcess // the running agent, if any
}

type agentProcess struct {
	cmd *exec.Cmd
	// logFrom is where this run's lines start in machine.agentLog.
	logFrom int
	done    chan struct{}
	err     error // how it exited, once done is closed
}

func newMachine(t *testing.T, name string) *machine {
	t.Helper()
	m := &machine{t: t, name: name, home: t.TempDir(), log: logFor(t)}
	t.Cleanup(func() {
		if m.agent != nil {
			m.Stop()
		}
	})
	return m
}

// lentMachine sets up what most tests start from: a service, and alice's
// machine paired with it, with the probe approved and the agent running.
func lentMachine(t *testing.T, options ...serviceOption) (*service, *machine) {
	t.Helper()
	svc := newService(t, options...)
	m := newMachine(t, "alice-mac")
	m.Pair(svc, "alice")
	m.Approve(svc.probe)
	m.Start()
	return svc, m
}

// ID is the service's ID for the machine.
func (m *machine) ID() string { return m.id }

// env is the environment every moil command runs with: only what it
// needs, so the developer's own settings don't leak in.
func (m *machine) env() []string {
	return []string{
		"MOIL_HOME=" + m.home,
		"MOIL_SECRETS=file",
		"MOIL_UV=" + uvBin,
		"HOME=" + os.Getenv("HOME"),
		"PATH=" + os.Getenv("PATH"),
		"TMPDIR=" + os.TempDir(),
		"LANG=C.UTF-8",
		"NO_COLOR=1",
	}
}

func (m *machine) command(ctx context.Context, args ...string) *exec.Cmd {
	cmd := exec.CommandContext(ctx, moilBin, args...)
	cmd.Env = m.env()
	cmd.WaitDelay = 5 * time.Second
	inReaperGroup(cmd)
	return cmd
}

// try runs a moil command to its end, with nothing on its stdin: moil
// never asks without a terminal.
func (m *machine) try(args ...string) (stdout, stderr string, err error) {
	m.t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), patience)
	defer cancel()
	var out, errOut bytes.Buffer
	cmd := m.command(ctx, args...)
	cmd.Stdout, cmd.Stderr = &out, &errOut
	err = cmd.Run()
	m.log.print(m.name, "$ moil "+strings.Join(args, " "))
	if stderr := strings.TrimSpace(errOut.String()); stderr != "" {
		for _, line := range strings.Split(stderr, "\n") {
			m.log.print(m.name, line)
		}
	}
	return out.String(), errOut.String(), err
}

// moil runs a moil command that must succeed, and returns its stdout.
func (m *machine) moil(args ...string) string {
	m.t.Helper()
	stdout, stderr, err := m.try(args...)
	if err != nil {
		m.t.Fatalf("moil %s: %v\n%s", strings.Join(args, " "), err, stderr)
	}
	return stdout
}

// Pair pairs the machine with svc, for owner: `moil pair`, then the
// owner confirms the code on the service.
func (m *machine) Pair(svc *service, owner string) {
	m.t.Helper()
	m.svc = svc
	m.PairWith(svc.link.URL()+"/moil", func(code string) {
		paired, err := svc.sdk.ConfirmPairing(context.Background(), code, owner)
		if err != nil {
			m.t.Fatalf("confirming pairing code %s: %v", code, err)
		}
		m.id = paired.ID
	})
}

// PairWith pairs the machine with the service at baseURL: `moil pair`,
// then confirm with the code it shows.
func (m *machine) PairWith(baseURL string, confirm func(code string)) {
	m.t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), patience)
	defer cancel()
	cmd := m.command(ctx, "pair", baseURL, "--no-browser", "--name", m.name)
	cmd.Stderr = m.log.writer(m.name)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		m.t.Fatal(err)
	}
	if err := cmd.Start(); err != nil {
		m.t.Fatal(err)
	}
	var shown []string
	scanner := bufio.NewScanner(stdout)
	for scanner.Scan() {
		line := scanner.Text()
		shown = append(shown, line)
		if code, ok := strings.CutPrefix(line, "Code: "); ok {
			confirm(code)
		}
		if id, ok := strings.CutPrefix(line, "Paired: "); ok {
			m.serviceID = id
		}
	}
	if err := cmd.Wait(); err != nil || m.serviceID == "" {
		m.t.Fatalf("moil pair %s: %v; it printed:\n%s", baseURL, err, strings.Join(shown, "\n"))
	}
}

// Heard reports whether the service it's paired with ever sent the
// machine s.
func (m *machine) Heard(s string) bool {
	m.t.Helper()
	return slices.ContainsFunc(m.tokens(), func(token string) bool { return m.svc.link.Heard(token, s) })
}

// Approve approves bundle for the service, as the owner would after
// reading it: `moil approve`.
func (m *machine) Approve(bundle *moil.Bundle) {
	m.t.Helper()
	m.ApproveHash(bundle.Hash())
}

// ApproveHash approves the bundle with this hash for the service.
func (m *machine) ApproveHash(hash string) {
	m.t.Helper()
	out := m.moil("approve", m.serviceID, hash, "--yes")
	if !strings.Contains(out, "Approved: "+hash) {
		m.t.Fatalf("moil approve didn't approve %s:\n%s", hash, out)
	}
}

// tokens returns the machine tokens the machine keeps, each of which it
// keeps with the base URL it was issued for.
func (m *machine) tokens() []string {
	m.t.Helper()
	data, err := os.ReadFile(filepath.Join(m.home, ".machine", "secrets.json"))
	if err != nil {
		m.t.Fatalf("reading the machine's tokens: %v", err)
	}
	var secrets map[string]string
	if err := json.Unmarshal(data, &secrets); err != nil || len(secrets) == 0 {
		m.t.Fatalf("the machine keeps no tokens: %v, %s", err, data)
	}
	var tokens []string
	for _, stored := range secrets {
		var issued struct {
			BaseURL string `json:"base_url"`
			Token   string `json:"token"`
		}
		if err := json.Unmarshal([]byte(stored), &issued); err != nil || issued.Token == "" || issued.BaseURL == "" {
			m.t.Fatalf("the machine keeps a token without the URL it was issued for: %v", err)
		}
		tokens = append(tokens, issued.Token)
	}
	return tokens
}

// Start runs `moil agent` and waits until it's connected to the service.
func (m *machine) Start() {
	m.t.Helper()
	if m.agent != nil {
		m.t.Fatalf("%s's agent is already running", m.name)
	}
	a := &agentProcess{logFrom: m.agentLog.len(), done: make(chan struct{})}
	a.cmd = m.command(context.Background(), "agent")
	a.cmd.Stderr = m.log.writer(m.name, m.agentLog.add)
	if err := a.cmd.Start(); err != nil {
		m.t.Fatal(err)
	}
	go func() {
		a.err = a.cmd.Wait()
		close(a.done)
	}()
	m.agent = a
	eventually(m.t, m.name+"'s agent to connect", func() bool {
		select {
		case <-a.done:
			m.agent = nil
			m.t.Fatalf("%s's agent exited: %v", m.name, a.err)
		default:
		}
		if m.svc != nil {
			return m.svc.Machine(m.id).Online()
		}
		_, connected := m.agentLog.find(a.logFrom, "connected to ")
		return connected
	})
}

// Stop stops the agent as its owner would: SIGTERM, which makes it
// interrupt the running job, report it, and exit.
func (m *machine) Stop() {
	m.t.Helper()
	if err := m.end(syscall.SIGTERM); err != nil {
		m.t.Errorf("%s's agent exited with %v after SIGTERM", m.name, err)
	}
}

// Kill kills the agent with SIGKILL, as a crash would. The processes of a
// running job are left behind.
func (m *machine) Kill() {
	m.t.Helper()
	m.end(syscall.SIGKILL)
}

func (m *machine) end(signal syscall.Signal) error {
	m.t.Helper()
	a := m.agent
	if a == nil {
		m.t.Fatalf("%s's agent isn't running", m.name)
	}
	m.agent = nil
	a.cmd.Process.Signal(signal)
	select {
	case <-a.done:
	case <-time.After(patience):
		a.cmd.Process.Kill()
		<-a.done
		m.t.Fatalf("%s's agent was still running %v after %v", m.name, patience, signal)
	}
	var exit *exec.ExitError
	if signal == syscall.SIGKILL && errors.As(a.err, &exit) {
		return nil
	}
	return a.err
}

// Logged waits until the running agent, or an earlier one, has logged a
// line containing s, and returns the line.
func (m *machine) Logged(s string) string {
	m.t.Helper()
	var line string
	eventually(m.t, m.name+"'s agent to log "+s, func() bool {
		var ok bool
		line, ok = m.agentLog.find(0, s)
		return ok
	})
	return line
}
