package wire

import "testing"

// The same cases as moil-proto's UrlPolicy tests, so that a service
// refuses at Submit what machines would refuse later.
func TestURLPolicy(t *testing.T) {
	remote := ServicePolicy("https://klisi.example.com/moil", false)
	local := ServicePolicy("http://127.0.0.1:8787/moil", false)
	insecure := ServicePolicy("https://klisi.example.com/moil", true)
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
