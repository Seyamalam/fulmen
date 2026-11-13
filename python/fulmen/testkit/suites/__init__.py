"""The bundled conformance suites — collected when pytest runs with
`--fulmen-adapter=pkg:Cls` (see `fulmen.testkit.plugin`).

Each test takes its interface fixture (`session_store`, `job_queue`, …)
as a parameter, so the same functions are callable directly —
`python/tests` exercises them against the built-in `Memory*` stores.
"""
