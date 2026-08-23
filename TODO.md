# TODO

- [ ] **collecta-cli: pull form definitions.** `GET /api/v1/sync/forms?since=<cursor>`
      still has no consumer. A `pull` command would store the returned forms and
      the cursor beside the queue file, and apply the `deleted` tombstones, so a
      device can pick up a form it was not shipped with.
