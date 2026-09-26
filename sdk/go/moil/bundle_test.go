package moil_test

import (
	"encoding/json"
	"os"
	"path/filepath"
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
	readJSON(t, filepath.Join(vectorsDir, "manifests.json"), &vectors)
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
	readJSON(t, filepath.Join(vectorsDir, "bundles.json"), &vectors)
	for _, c := range vectors.Valid {
		t.Run(c.Dir, func(t *testing.T) {
			b, err := moil.LoadBundle(os.DirFS(filepath.Join(vectorsDir, "bundles", c.Dir)))
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
		data, err := os.ReadFile(filepath.Join(vectorsDir, "bundles", dir, name))
		if err != nil {
			t.Fatal(err)
		}
		files[name] = data
	}
	return files
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
