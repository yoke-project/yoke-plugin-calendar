# Trying calendar

This directory is an application project that uses the Plugin in `..`: `yoke-compose init` made it, and
`add plugin` took `online-accounts` and then the Plugin, its need `accounts` bound to the first. Nothing
here is the Plugin, and taking the Plugin into another application never takes this directory.

| Path | What it is |
| --- | --- |
| `yoke.yaml` | the application: the units `online-accounts` and `calendar`, the second's need bound to the first, and the channel `panel`, a local socket for one client |
| `demo/` | a directory of `.ics` with events that recur every hour and every working day, so there is always something next |
| `follow/` | an interface client: it asks the unit's sources, follows its stream and every event, and prints each as one line |
| `run.sh` | packages the application, installs it as the instance `cal` in a world of its own under `.world/`, grants what both Plugins declare, and follows `calendar` |

`online-accounts` is taken from the Plugin project beside this repository,
`../../yoke-plugin-online-accounts`, until it carries a tag to take it at; `--online-accounts <dir>` names
another. The package is made from a copy under `.world/`, so `run.sh` leaves this directory as it is written:
the demo directory and every `--source` are added to the copy's arguments. An installed version is never
replaced, so a run that changed the package installs it as `<version>-try-<digest>` and moves the instance to
it. `--fresh` forgets the world: the instance, its accounts and what was kept of every source.

The programs of Yoke 0.17.0 or later are installed from the module proxy:

```sh
for p in yoke yoke-compose yoke-core yoke-guardian yoke-auth; do
  go install "github.com/yoke-project/yoke/cmd/$p@latest"
done
```

```sh
./run.sh                                                  # the demo alone
./run.sh --source holidays=ics:https://example.org/it.ics --seconds 10
./run.sh --clients ~/clients.json --source home=google:ada@example.org
```

A Microsoft or Google source needs its account signed in, in `online-accounts`, asking for the calendar's
scope — the line `run.sh` prints has the instance's world:

    yoke unit command online-accounts oauth.login --input - --instance cal <<< '{"provider":"google","scopes":["https://www.googleapis.com/auth/calendar.readonly"]}'
    yoke unit command calendar calendar.refresh --instance cal
    yoke unit ask calendar calendar.days --input - --instance cal <<< '{"days":3}'
