# Committed baselines

`results.json` files that `bench/compare.sh` measures new runs against. One
per machine class, named after the runner it was recorded on.

| file | recorded on |
|---|---|
| `ci-x86_64.json` | the GitHub `ubuntu-latest` runner, by `.github/workflows/bench.yml` |

A baseline is only meaningful on the hardware that produced it — `compare.sh`
refuses to compare across CPU models for exactly that reason — so these are
not "the project's numbers". They are a tripwire for one specific runner.

## Recording or refreshing one

Deliberately a manual, reviewable act, not something a workflow does on its
own. A baseline that updates itself ratchets silently: each run becomes the
new normal, and a 3% regression every month is invisible forever.

```bash
./bench/run.sh micro --workload many-small --scale 0.05 --reps 5 --yes
cp bench/results/<host>/latest/results.json bench/baseline/ci-x86_64.json
```

Commit it with the change that justifies it, and say in the commit message
which direction the numbers moved and why. A refresh that follows a real
optimisation is a record of the win; a refresh that follows a regression needs
to explain itself.

The parameters above (`micro`, `many-small`, `--scale 0.05`, `--reps 5`) must
match what the workflow runs, or `compare.sh` will find no cases in common and
say so.

## When the runner hardware changes

GitHub rotates runner CPU generations. `compare.sh` will fail loudly with a
CPU mismatch rather than reporting a fake regression; the fix is to record a
fresh baseline on the new hardware in its own commit, so the discontinuity in
the numbers has an explanation attached to it.
