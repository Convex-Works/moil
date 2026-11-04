# Trust model

moil runs arbitrary code. Approving a bundle hash means "I trust exactly
this code, and the exact dependencies it pins", in the same way installing
an app means trusting its author.

- A compromised service can't swap the code: the machine verifies the hash
  locally before every run, and shows only what it verified.
- Nor the dependencies: the lockfile must pin every artifact by hash and
  come from https sources, and packages that have wheels install only
  from those wheels. A package that must be built from source is marked in
  the review, because its build tools aren't pinned.
- The review marks characters that could hide code from a reader:
  carriage returns, escape sequences, bidirectional overrides and
  invisible characters.
- A compromised service *can* send malicious inputs to an approved script,
  so scripts should treat inputs as untrusted. It can't aim a job's
  downloads and uploads at services that listen only on the owner's
  machine: plain `http` to loopback is only for services on loopback.
- Whoever runs a machine can see the job's inputs. Services must only offer
  sensitive data (e.g. meeting audio) to machines their policy allows,
  typically the data owner's own.
- moil has no sandbox, and the review window says so. A script runs with
  the owner's permissions, so it can reach `~/.moil` and moil's control
  socket.
- Hooks are your own code: nobody approves them, and they run as you.
  What they're given comes partly from the service (a job's title and
  params, its outputs' names and contents), so a hook should treat it as
  untrusted. Whoever can push to a service folder you keep in git can
  approve bundles and add hooks on your machines.
