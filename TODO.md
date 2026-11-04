# TODO

## From tsk-tsk

Design, alternatives and costs: [docs/dev/tsk-tsk.md](docs/dev/tsk-tsk.md).

- [ ] **Sealed containers reach host services.** README qualified, Linux warning and live test done. Open: a host `INPUT` rule admitting only the relay's port (root, per firewall backend); macOS listener check; Apple's `container` unmeasured (`make live` measures it); UDP, such as a host resolver on `0.0.0.0:53`, is not probed.
- [ ] **Return work as a git bundle.** Done as `run --clone`, opt-in. Open: run the return step against a real image (tested only with the stand-in docker); make it pma's default; a lock if two runs fetch into one repository at once; LFS and submodules are untried.
- [ ] **One network per run.** `sanduk-net-<run-id>`, deleted with the run's containers. Or, Docker only: `enable_icc=false` on the shared network.
- [ ] **Registries for sealed runs.** The relay forwards GET-only to fixed registry hosts. First: does a caller need it?
