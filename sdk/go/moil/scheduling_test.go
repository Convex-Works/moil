package moil_test

import (
	"sync"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

// A job is only ever offered to machines the service's policy allows:
// alice's recording never reaches bob's machine, even while bob's is the
// only one online.
func TestEligibilityDecidesWhoIsOffered(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	alice := moiltest.Pair(t, srv, "alice")
	bob := moiltest.Pair(t, srv, "bob")
	alice.Approve(bundle)
	bob.Approve(bundle)
	bob.Connect()

	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("alice")})
	bob.Sync()
	if offers := bob.Offers(); len(offers) != 0 {
		t.Fatalf("bob's machine was offered alice's job: %+v", offers)
	}
	if run.State() != moil.Queued {
		t.Fatalf("state %s with no eligible machine online", run.State())
	}

	alice.Connect() // the job has waited, without a time limit
	alice.NextAttempt().Succeed(nil)
	if res := succeeded(t, run); res.Machine != alice.ID() {
		t.Fatalf("ran on %s", res.Machine)
	}
	bob.Sync()
	if offers := bob.Offers(); len(offers) != 0 {
		t.Fatalf("bob's machine was offered alice's job: %+v", offers)
	}
}

// A machine is offered a job only once its owner approved the bundle, and
// only while it's idle.
func TestOnlyIdleMachinesThatApprovedTheBundleAreOffered(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	other := loadBundle(t, srv, "crlf-unicode")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(other)
	m.Connect()

	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	m.Sync()
	if offers := m.Offers(); len(offers) != 0 {
		t.Fatalf("offered a bundle the owner didn't approve: %+v", offers)
	}

	m.SetState(moil.Paused)
	m.Approve(bundle)
	m.Sync()
	if offers := m.Offers(); len(offers) != 0 {
		t.Fatalf("offered a job while paused: %+v", offers)
	}

	m.SetState(moil.Idle)
	a := m.NextAttempt()
	if offers := m.Offers(); len(offers) != 1 || offers[0].JobID != run.ID() || offers[0].BundleHash != bundle.Hash() {
		t.Fatalf("offers %+v", offers)
	}
	a.Succeed(nil)
	succeeded(t, run)

	// Revoking the approval stops the offers again.
	m.Revoke(bundle)
	m.Sync()
	next := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	m.Sync()
	if offers := m.Offers(); len(offers) != 1 {
		t.Fatalf("offered a revoked bundle: %+v", offers)
	}
	next.Cancel()
}

// A machine holds at most one open offer, and one attempt: a second job
// waits for it.
func TestOneOfferAndOneAttemptPerMachine(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	release := make(chan struct{})
	t.Cleanup(func() {
		select {
		case <-release:
		default:
			close(release)
		}
	})
	m.OnOffer(func(moiltest.Offer) moiltest.Answer { <-release; return moiltest.Bid })
	m.Connect()

	first := submit(t, srv, moil.Job{Title: "first", Bundle: bundle, Eligible: moil.AnyMachine})
	second := submit(t, srv, moil.Job{Title: "second", Bundle: bundle, Eligible: moil.AnyMachine})
	close(release)
	a := m.NextAttempt()
	if a.JobID != first.ID() {
		t.Fatalf("the first job submitted must be offered first; got %s", a.Title)
	}
	m.Sync()
	if offers := m.Offers(); len(offers) != 1 {
		t.Fatalf("a busy machine was offered another job: %+v", offers)
	}
	if second.State() != moil.Queued {
		t.Fatalf("second job %s", second.State())
	}
	a.Succeed(nil)
	m.NextAttempt().Succeed(nil)
	succeeded(t, second)
}

// orderBids makes first bid at once and second bid only after the service
// has processed first's bid, so the bid order is known. Call it before the
// job is offered, and call release with the job's ID after.
func orderBids(t *testing.T, first, second *moiltest.Machine) (release func(jobID string)) {
	firstBid := make(chan struct{})
	var once sync.Once
	t.Cleanup(func() { once.Do(func() { close(firstBid) }) })
	first.OnOffer(func(moiltest.Offer) moiltest.Answer { return moiltest.Bid })
	second.OnOffer(func(moiltest.Offer) moiltest.Answer { <-firstBid; return moiltest.Bid })
	return func(jobID string) {
		t.Helper()
		if o := first.NextOffer(); o.JobID != jobID || o.Answer != moiltest.Bid {
			t.Fatalf("%s answered %+v, want a bid for %s", first.ID(), o, jobID)
		}
		first.Sync()
		once.Do(func() { close(firstBid) })
	}
}

// Without a preference the earliest bid wins; with one, the preferred
// machine wins even though it bid last.
func TestPreferenceAmongBidders(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	laptop := moiltest.Pair(t, srv, "alice", moiltest.WithName("laptop"))
	desktop := moiltest.Pair(t, srv, "alice", moiltest.WithName("desktop"), moiltest.WithGPU("RTX 4090", 24<<30))
	for _, m := range []*moiltest.Machine{laptop, desktop} {
		m.Approve(bundle)
		m.Connect()
	}

	release := orderBids(t, laptop, desktop)
	plain := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	release(plain.ID())
	laptop.NextAttempt().Succeed(nil)
	if res := succeeded(t, plain); res.Machine != laptop.ID() {
		t.Fatalf("without Prefer the first bid must win; %s won", res.Machine)
	}
	laptop.Sync() // the laptop is idle again

	release = orderBids(t, laptop, desktop)
	gpu := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prefer: func(m moil.Machine) int {
		return len(m.GPUs)
	}})
	release(gpu.ID())
	desktop.NextAttempt().Succeed(nil)
	if res := succeeded(t, gpu); res.Machine != desktop.ID() {
		t.Fatalf("Prefer must beat bid order; %s won", res.Machine)
	}
}

// An offer that isn't answered in time is decided without that machine,
// which isn't offered the job again until it reports a change.
func TestUnansweredOffersExpire(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.BidWindow = 50 * time.Millisecond })
	bundle := loadBundle(t, srv, "minimal")
	slow := moiltest.Pair(t, srv, "alice")
	slow.Approve(bundle)
	slow.OnOffer(func(moiltest.Offer) moiltest.Answer { return moiltest.Ignore })
	slow.Connect()

	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	if o := slow.NextOffer(); o.Expires != 50*time.Millisecond {
		t.Fatalf("offer expires in %v, want the bid window", o.Expires)
	}
	fast := moiltest.Pair(t, srv, "alice")
	fast.Approve(bundle)
	fast.Connect() // offered once the silent machine's window has passed
	fast.NextAttempt().Succeed(nil)
	succeeded(t, run)
	slow.Sync()
	if n := len(slow.Offers()); n != 1 {
		t.Fatalf("the silent machine got %d offers, want 1", n)
	}
}

// A policy that panics excludes the machine instead of killing the
// scheduler.
func TestPanickingPoliciesAreContained(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	bad := submit(t, srv, moil.Job{Bundle: bundle, Eligible: func(moil.Machine) bool { panic("policy bug") }})
	good := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prefer: func(moil.Machine) int { panic("ranking bug") }})
	m.NextAttempt().Succeed(nil)
	succeeded(t, good)
	if bad.State() != moil.Queued {
		t.Fatalf("the job with a panicking policy is %s", bad.State())
	}
}
