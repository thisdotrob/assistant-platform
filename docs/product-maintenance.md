# Product Maintenance

Product-maintenance changes add, remove, or upgrade modules in a product repo.

These changes happen through normal code review:

- change product dependencies or manifests;
- update product profile wiring;
- regenerate lockfiles when dependency management exists;
- run platform/product conformance checks;
- upgrade runtime instances through scripted upgrade/setup paths.

Runtime setup and agents must not mutate product source, add module code, install packages, or edit checked-in profile prompts.

