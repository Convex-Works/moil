package moil_test

import (
	"encoding/json"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"testing/fstest"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

const vectorsDir = "../../../spec/vectors"

// Every manifest in the vectors is accepted or rejected exactly as marked,
// so a bundle the service advertises is one machines accept, and vice versa.
func TestManifestVectors(t *testing.T) {
	var vectors struct {
		Cases []struct {
			Case     string `json:"case"`
			Valid    bool   `json:"valid"`
			Manifest string `json:"manifest"`
		} `json:"cases"`
	}
	readJSON(t, vectorPath(t, "manifests.json"), &vectors)
	if len(vectors.Cases) < 20 {
		t.Fatalf("only %d manifest vectors", len(vectors.Cases))
	}
	for _, c := range vectors.Cases {
		t.Run(c.Case, func(t *testing.T) {
			m, err := moil.ParseManifest([]byte(c.Manifest))
			switch {
			case c.Valid && err != nil:
				t.Fatalf("rejected a valid manifest: %v", err)
			case !c.Valid && err == nil:
				t.Fatalf("accepted an invalid manifest as %+v", m)
			}
		})
	}
}

func TestManifestStrictnessBeyondTheVectors(t *testing.T) {
	for _, c := range []struct{ name, manifest string }{
		{"key differing only in case", `{"moil": 1, "Name": "echo", "name": "echo", "version": "1"}`},
		{"duplicate key in resources", `{"moil": 1, "name": "echo", "version": "1", "resources": {"gpu": "none", "gpu": "required"}}`},
		{"assets null", `{"moil": 1, "name": "echo", "version": "1", "assets": null}`},
		{"resources as array", `{"moil": 1, "name": "echo", "version": "1", "resources": []}`},
		{"trailing data", `{"moil": 1, "name": "echo", "version": "1"} {}`},
		{"moil as float", `{"moil": 1.0, "name": "echo", "version": "1"}`},
		{"asset url without host", `{"moil": 1, "name": "echo", "version": "1", "assets": {"a": {"url": "https:///a", "sha256": "` + strings.Repeat("a", 64) + `"}}}`},
		{"asset url with an empty label", `{"moil": 1, "name": "echo", "version": "1", "assets": {"a": {"url": "https://example..com/a", "sha256": "` + strings.Repeat("a", 64) + `"}}}`},
		{"asset url with a signed port", `{"moil": 1, "name": "echo", "version": "1", "assets": {"a": {"url": "https://example.com:+443/a", "sha256": "` + strings.Repeat("a", 64) + `"}}}`},
		{"asset url with a broken percent-encoding", `{"moil": 1, "name": "echo", "version": "1", "assets": {"a": {"url": "https://example.com/a%2", "sha256": "` + strings.Repeat("a", 64) + `"}}}`},
		{"not UTF-8", "{\"moil\": 1, \"name\": \"echo\", \"version\": \"1\xff\"}"},
		{"low surrogate alone", `{"moil": 1, "name": "echo", "version": "1", "description": "\udc00"}`},
		{"high surrogate before a letter", `{"moil": 1, "name": "echo", "version": "1", "description": "\ud83d\u0041"}`},
		{"unpaired surrogate in a key", `{"moil": 1, "name": "echo", "version": "1", "assets": {"\ud800": {"url": "https://e.com/a", "sha256": "` + strings.Repeat("a", 64) + `"}}}`},
	} {
		t.Run(c.name, func(t *testing.T) {
			if m, err := moil.ParseManifest([]byte(c.manifest)); err == nil {
				t.Fatalf("accepted as %+v", m)
			}
		})
	}
	m, err := moil.ParseManifest([]byte(`{"moil": 1, "name": "echo", "version": "1", "description": null, "resources": {"gpu": null}}`))
	if err != nil {
		t.Fatalf("null optional fields are absent fields: %v", err)
	}
	if m.Description != "" || m.Resources.GPU != "" {
		t.Fatalf("got %+v", m)
	}
	// Escaped surrogate pairs are characters, and an escaped backslash
	// before a u is just text.
	m, err = moil.ParseManifest([]byte(`{"moil": 1, "name": "echo", "version": "1", "description": "\ud83d\ude00 \\ud800"}`))
	if err != nil {
		t.Fatalf("a surrogate pair was refused: %v", err)
	}
	if m.Description != "😀 \\ud800" {
		t.Fatalf("description %q", m.Description)
	}
}

// Valid bundles hash to their listed hash; every invalid case is rejected.
func TestBundleVectors(t *testing.T) {
	var vectors struct {
		Valid []struct {
			Dir  string `json:"dir"`
			Hash string `json:"hash"`
		} `json:"valid"`
		Invalid []struct {
			Case   string            `json:"case"`
			Base   string            `json:"base"`
			Set    map[string]string `json:"set"`
			Remove []string          `json:"remove"`
		} `json:"invalid"`
	}
	readJSON(t, vectorPath(t, "bundles.json"), &vectors)
	for _, c := range vectors.Valid {
		t.Run(c.Dir, func(t *testing.T) {
			b, err := moil.LoadBundle(os.DirFS(vectorPath(t, "bundles", c.Dir)))
			if err != nil {
				t.Fatal(err)
			}
			if b.Hash() != c.Hash {
				t.Fatalf("hash %s, want %s", b.Hash(), c.Hash)
			}
		})
	}
	for _, c := range vectors.Invalid {
		t.Run(c.Case, func(t *testing.T) {
			files := readBundleFiles(t, c.Base)
			for name, text := range c.Set {
				files[name] = []byte(text)
			}
			for _, name := range c.Remove {
				delete(files, name)
			}
			if b, err := moil.NewBundle(files); err == nil {
				t.Fatalf("accepted as %v", b)
			}
		})
	}
}

// Every lockfile in the vectors makes a valid bundle or not as marked, and
// the packages machines would build from source are the ones listed.
func TestLockfileVectors(t *testing.T) {
	var vectors struct {
		Cases []struct {
			Case             string   `json:"case"`
			Valid            bool     `json:"valid"`
			Lockfile         string   `json:"lockfile"`
			BuildsFromSource []string `json:"builds_from_source"`
		} `json:"cases"`
	}
	readJSON(t, vectorPath(t, "lockfiles.json"), &vectors)
	if len(vectors.Cases) < 15 {
		t.Fatalf("only %d lockfile vectors", len(vectors.Cases))
	}
	base := readBundleFiles(t, "minimal")
	for _, c := range vectors.Cases {
		t.Run(c.Case, func(t *testing.T) {
			files := maps.Clone(base)
			files[moil.LockFile] = []byte(c.Lockfile)
			b, err := moil.NewBundle(files)
			switch {
			case c.Valid && err != nil:
				t.Fatalf("rejected a valid lockfile: %v", err)
			case !c.Valid && err == nil:
				t.Fatalf("accepted an invalid lockfile, with packages %+v", b.Packages())
			case !c.Valid:
				return
			}
			built := []string{}
			for _, p := range b.Packages() {
				if p.BuildsFromSource {
					built = append(built, p.Name)
				}
			}
			if !slices.Equal(built, c.BuildsFromSource) {
				t.Fatalf("builds %v from source, want %v", built, c.BuildsFromSource)
			}
		})
	}
}

func TestBundleListsItsPackages(t *testing.T) {
	b := vectorBundle(t, "crlf-unicode")
	want := []moil.Package{{Name: "idna", Version: "3.20", SourceKind: "registry", Source: "https://pypi.org/simple"}}
	if got := b.Packages(); !slices.Equal(got, want) {
		t.Fatalf("packages %+v, want %+v", got, want)
	}
	b.Packages()[0].Name = "changed"
	if b.Packages()[0].Name != "idna" {
		t.Fatal("a caller changed the bundle's packages")
	}

	// The attack the rules close: an index the service controls serves an
	// unhashed sdist, which it could change after approval.
	files := readBundleFiles(t, "minimal")
	files[moil.LockFile] = []byte(`
[[package]]
name = "evilpkg"
version = "0.1"
source = { registry = "https://evil.example.com/simple" }
sdist = { url = "https://evil.example.com/evilpkg-0.1.tar.gz", size = 900 }
`)
	_, err := moil.NewBundle(files)
	if want := "package evilpkg: https://evil.example.com/evilpkg-0.1.tar.gz has no SHA-256 hash, so it isn't pinned"; err == nil || !strings.HasSuffix(err.Error(), want) {
		t.Fatalf("err = %v, want one ending %q", err, want)
	}
}

// Line endings and non-ASCII text are hashed as the raw bytes they are:
// normalizing CRLF would change what the owner approved.
func TestBundleHashIsOverRawBytes(t *testing.T) {
	files := readBundleFiles(t, "crlf-unicode")
	if !strings.Contains(string(files["job.py"]), "\r\n") {
		t.Fatal("the vector lost its CRLF line endings; check .gitattributes")
	}
	original, err := moil.NewBundle(files)
	if err != nil {
		t.Fatal(err)
	}
	files["job.py"] = []byte(strings.ReplaceAll(string(files["job.py"]), "\r\n", "\n"))
	normalized, err := moil.NewBundle(files)
	if err != nil {
		t.Fatal(err)
	}
	if normalized.Hash() == original.Hash() {
		t.Fatal("CRLF and LF versions hash the same")
	}
}

func TestLoadBundleIgnoresOtherFilesButNewBundleDoesNot(t *testing.T) {
	files := readBundleFiles(t, "minimal")
	fsys := fstest.MapFS{"README.md": {Data: []byte("# docs")}}
	for name, data := range files {
		fsys[name] = &fstest.MapFile{Data: data}
	}
	loaded, err := moil.LoadBundle(fsys)
	if err != nil {
		t.Fatalf("a README next to the bundle must not matter: %v", err)
	}
	if loaded.Name() != "minimal" || loaded.Version() != "1.0.0" || loaded.Description() == "" {
		t.Fatalf("manifest fields: %q %q %q", loaded.Name(), loaded.Version(), loaded.Description())
	}
	if got := string(loaded.File("job.py")); got != string(files["job.py"]) {
		t.Fatalf("File(job.py) = %q", got)
	}
	files["README.md"] = []byte("# docs")
	if _, err := moil.NewBundle(files); err == nil {
		t.Fatal("NewBundle accepted a fourth file")
	}
}

func TestBundleRejectsNonUTF8AndOversizedFiles(t *testing.T) {
	files := readBundleFiles(t, "minimal")
	files["job.py"] = []byte{0xff, 0xfe}
	if _, err := moil.NewBundle(files); err == nil || !strings.Contains(err.Error(), "UTF-8") {
		t.Fatalf("err = %v", err)
	}
	files = readBundleFiles(t, "minimal")
	files["job.py"] = append(files["job.py"], []byte("#"+strings.Repeat("x", moil.MaxBundleBytes)+"\n")...)
	if _, err := moil.NewBundle(files); err == nil || !strings.Contains(err.Error(), "8 MiB") {
		t.Fatalf("err = %v", err)
	}
}

func readBundleFiles(t *testing.T, dir string) map[string][]byte {
	t.Helper()
	files := map[string][]byte{}
	for _, name := range []string{moil.ScriptFile, moil.LockFile, moil.ManifestFile} {
		data, err := os.ReadFile(vectorPath(t, "bundles", dir, name))
		if err != nil {
			t.Fatal(err)
		}
		files[name] = data
	}
	return files
}

// vectorPath returns the path of a file in spec/vectors, or skips the test
// when the vectors aren't there, as when the SDK was fetched as a module.
func vectorPath(t *testing.T, elem ...string) string {
	t.Helper()
	if _, err := os.Stat(vectorsDir); err != nil {
		t.Skipf("the shared test vectors aren't at %s (%v); run the SDK's tests from a checkout of the moil repository", vectorsDir, err)
	}
	return filepath.Join(append([]string{vectorsDir}, elem...)...)
}

func readJSON(t *testing.T, path string, v any) {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, v); err != nil {
		t.Fatal(err)
	}
}
