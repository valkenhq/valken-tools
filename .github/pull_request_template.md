## What

<!-- One paragraph: what changes and why. Link the issue if there is one. -->

## How to check it

<!-- What a reviewer can run or look at. `make ci` is assumed. -->

## Checklist

- [ ] `make ci` passes locally
- [ ] Version bumped (`make bump-patch` / `bump-minor` / `bump-major`), or the PR is labelled `release:skip`
- [ ] `CHANGELOG.md` has a line under *Unreleased* (or nothing user-visible changed)
- [ ] Still read-only: no new call creates, patches or deletes anything outside `k8s::write`
- [ ] Nothing sensitive can reach a log, an error or the UI (Secret values, env values, tokens, exec-plugin environments); kube errors are displayed through `k8s::error::describe`
- [ ] New cluster access goes through a seam (`ClusterSource`, `MetricsSource`, `LogSource`, `SecuritySource`) and has a test over the fakes
- [ ] If an AI agent wrote any of this, I have read and understood every line and I'm answerable for it (see CONTRIBUTING.md)
