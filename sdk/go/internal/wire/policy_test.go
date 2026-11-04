package wire

import "testing"

// The same cases as moil-proto's UrlPolicy tests, so that a service
// refuses at Submit what machines would refuse later.
func TestURLPolicy(t *testing.T) {
	remote := ServicePolicy("https://tide.example.com/moil", false)
	local := ServicePolicy("http://127.0.0.1:8787/moil", false)
	insecure := ServicePolicy("https://tide.example.com/moil", true)
	for _, c := range []struct {
		policy  URLPolicy
		url     string
		allowed bool
	}{
		{remote, "https://s3.example.com/a", true},
		{remote, "http://s3.example.com/a", false},
		{remote, "http://127.0.0.1:9000/a", false},
		{remote, "http://localhost/a", false},
		{remote, "http://[::1]/a", false},
		{remote, "ftp://example.com/a", false},
		{remote, "file:///etc/passwd", false},
		{remote, "https:///a", false},
		{remote, "/relative", false},
		{local, "http://127.0.0.1:8787/files/x", true},
		{local, "http://127.200.0.1/a", true},
		{local, "http://localhost./a", true},
		{local, "http://LOCALHOST/a", true},
		{local, "http://[::1]:1/a", true},
		{local, "https://e.com/a", true},
		{local, "http://example.com/a", false},
		{local, "http://[::ffff:127.0.0.1]/a", false},
		{local, "http://128.0.0.1/a", false},
		{local, "http://localhost.example.com/a", false},
		{insecure, "http://minio:9000/a", true},
		{insecure, "file:///etc/passwd", false},
		{insecure, "ftp://example.com/a", false},
	} {
		if err := c.policy.Check(c.url); (err == nil) != c.allowed {
			t.Errorf("%+v.Check(%q) = %v, want allowed %v", c.policy, c.url, err, c.allowed)
		}
	}
	if err := remote.Check("http://localhost:6379/"); err == nil || err.Error() != "uses http to localhost, on the machine, for a service that isn't; only https is allowed" {
		t.Errorf("loopback http from a remote service: %v", err)
	}
}

// The same cases as moil-agent's check of confirmation pages, so that
// moiltest refuses to pair where machines would.
func TestCheckPairingPage(t *testing.T) {
	tide, local := "https://tide.example.com/moil", "http://127.0.0.1:8787/moil"
	for _, c := range []struct {
		base, page string
		allowed    bool
	}{
		{tide, "https://tide.example.com/machines/pair?code=WDJB-MJHT", true},
		{tide, "https://TIDE.example.com:443/machines", true},
		{"https://tide.example.com/", "https://tide.example.com", true},
		{local, "http://127.0.0.1:8787/pair", true},
		{"http://[::1]:8787/moil", "http://[0:0::1]:8787/pair", true},
		// A relay, passing on the page of the service it relays to.
		{"https://evil.example/moil", "https://tide.example.com/machines/pair?code=WDJB-MJHT", false},
		// Other hosts, however related.
		{tide, "https://machines.tide.example.com/pair", false},
		{tide, "https://example.com/pair", false},
		{"http://localhost:8787/moil", "http://127.0.0.1:8787/pair", false},
		// Other ports.
		{tide, "https://tide.example.com:8443/pair", false},
		{local, "http://127.0.0.1:8788/pair", false},
		// Other schemes.
		{tide, "http://tide.example.com/pair", false},
		{local, "https://127.0.0.1:8787/pair", false},
		// A user name that reads like the service's host.
		{tide, "https://tide.example.com@evil.example/pair", false},
		{tide, "https://tide.example.com:443@evil.example/pair", false},
		{"https://evil.example/moil", "https://tide.example.com@evil.example/pair", false},
		{"https://evil.example/moil", "https://owner:secret@evil.example/pair", false},
		// Pages that aren't on the web.
		{tide, "file:///Applications/Calculator.app", false},
		{tide, "javascript:alert(1)", false},
		{tide, "/machines/pair", false},
		{tide, "", false},
	} {
		if err := CheckPairingPage(c.base, c.page); (err == nil) != c.allowed {
			t.Errorf("CheckPairingPage(%q, %q) = %v, want allowed %v", c.base, c.page, err, c.allowed)
		}
	}
	err := CheckPairingPage("https://evil.example/moil", "https://tide.example.com/machines/pair")
	if err == nil || err.Error() != `"https://tide.example.com/machines/pair" is on https://tide.example.com:443, not on https://evil.example:443, the origin of the base URL` {
		t.Errorf("a relayed page: %v", err)
	}
}
