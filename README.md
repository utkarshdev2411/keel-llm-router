# Keel

Keel is a load balancer built specifically for LLM-serving workloads.

Most load balancers were designed for traditional web traffic, where requests are roughly
uniform in cost and a simple request-count or connection-count heuristic is enough to keep
backends balanced. LLM inference doesn't fit that assumption: a single request can produce a
one-line answer or several pages of generated text, and the difference in actual work done
by the backend can span two to three orders of magnitude. A balancer that only counts
requests is blind to this, and it shows up as one backend quietly getting overloaded with
expensive work while others sit idle.

Keel is being built to route on actual backend cost rather than request count, so that traffic
distributes according to real load instead of just request volume.

## Status

Early development. Design and planning are underway; the router itself is not yet functional.

## Goals

- Route LLM inference traffic based on the real cost of serving each request, not just how
  many requests a backend is holding
- Work as a drop-in reverse proxy in front of standard OpenAI-compatible inference servers,
  with no changes required on the backend or the client
- Stay lightweight and fast enough that the router itself is never the bottleneck
- Be benchmarked openly against common load-balancing approaches, with reproducible results

## Tech stack

Written in Rust, chosen for its performance characteristics and low-overhead concurrency
model, both of which matter for a proxy sitting in the hot path of every request.

## License

TBD.

## Status of this repository

This repository is currently private during early development. It will be made public and
open-sourced once the project reaches a usable state.
