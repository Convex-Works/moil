# moil's home

moil lends this computer to web services its owner uses, such as a meeting
app that wants its recordings transcribed. A service sends jobs, and moil
runs only the job bundles the owner approved for that service, one job at
a time, then sends the results back.

This folder holds everything moil acts on, as plain files that a person,
an editor, git or an LLM can read. moil wrote this README, and rewrites it
whenever its version of the text differs, so keep notes of your own in
another file.

## What's where

    README.md                 this file
    .gitignore                keeps this machine's own files out of git
    settings.json             this machine's preferences
    logs/moil.log             what moil did
    services/<id>/            one folder per service, named by its ID
      service.json            the service, and the bundles approved for it
      pairing.json            this machine's pairing with the service
      .gitignore              keeps pairing.json and logs/ out of git
      bundles/<name>/<hash>/  each approved bundle's three files, read-only
      hooks/<name>            the owner's program to run after that bundle's jobs
      logs/<name>.log         what that hook printed
    .machine/                 moil's own

You, an editor or git may change `settings.json`, each `service.json`,
anything under `hooks/`, and the `.gitignore` files. The rest is moil's:
`pairing.json`, `bundles/`, `logs/` and this README. `.machine/` holds
working directories, caches, the tokens when there is no keychain, and
the lock and socket of the running moil; its layout may change between
versions, and nothing else should depend on it.

A running moil notices changes to these files within a few seconds, and
goes by them as if they were made through moil: an approval added or
removed is announced to its service, and a removed one stops its running
job; deleting a `pairing.json` disconnects the service and deletes the
token; `paused` takes effect at once. If a file turns invalid, moil
reports it and goes on with what it read from it before.

moil writes JSON pretty-printed and replaces files whole, so a reader
never sees half of one. Times are in UTC, such as `2026-09-27T22:41:07Z`.
Its JSON files are strict: a file with a field moil doesn't know, or a key
given twice, is invalid, so that a typo is reported rather than ignored.

## Settings

`settings.json` may set `paused` (take no new jobs), `machine_name` (what
services call this computer), and, for developing bundles,
`allow_insecure_http` and `keep_work_dirs`. Every field is optional. moil
refuses to start with an invalid `settings.json` rather than guess.

## Services and approvals

A service's ID names its folder. moil derives it from the service's
address when this machine first pairs with it. `service.json` says which
service the folder is for (`base_url`, `name`) and lists the bundles the
owner approved for it (`approvals`). `pairing.json` records this
machine's pairing: the address it paired with, and the service's ID for
this machine. The token that proves this machine to the service is in the
keychain, or in `.machine/`, never in the service's folder, and moil sends
it only to the address it paired with. If `service.json` names another
address, the service isn't paired until this machine pairs with that one.

A bundle is the service's code: `job.py`, its lockfile `job.py.lock`, and
`manifest.json`, named by the SHA-256 hash of the three. Approving a
bundle writes its files, read-only, into `bundles/<name>/<hash>/` and adds
the approval to `service.json`. An approval counts only while that folder
holds the three files, they hash to the approval's hash, and the
manifest's name and version are the approval's; moil ignores any other
approval and reports it. moil runs a bundle only from a copy it verified
against the approval, and checks that copy again before every run, so
editing an approved bundle's files can't change what runs: its jobs are
refused until the files match again.

`moil services` lists the services this home knows, `moil review` shows
what a service offers, and `moil approve` and `moil revoke` change what is
approved. A folder without a `pairing.json`, such as one cloned from git,
is a service this machine knows but isn't paired with: `moil pair` pairs
with it and keeps its approvals and hooks. `moil unpair` tells the service
to forget this machine and deletes the pairing and token, but keeps the
folder; delete the folder to forget its approvals and hooks too.

## Hooks

A hook is the owner's own program, at `services/<id>/hooks/<bundle name>`.
After a job of that bundle for that service succeeds here, moil runs the
hook with the job's outputs. Nobody approves a hook: it runs as the owner,
and removing the file turns it off. `moil hooks --help` explains what a
hook gets and how it runs.

## Keeping the home in git

The `.gitignore` files keep out what belongs to this machine alone:
`.machine/`, `settings.json`, each `pairing.json`, and the logs. So the
whole home can be a git repository, or each service's folder one of its
own, and other machines can clone it and pair with its services.

Whoever can change a service's folder can approve bundles for it and add
hooks. A folder kept in git extends that power to whoever can push to its
repository.

## More

`moil --help` lists the commands: the `moil` command does everything the
app does. `moil status` shows what moil is doing, and what it finds wrong
with this folder.
