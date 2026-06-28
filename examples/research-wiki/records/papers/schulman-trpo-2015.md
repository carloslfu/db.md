---
type: paper
id: schulman-trpo-2015
created: 2026-04-21T10:05:00Z
updated: 2026-05-23T14:25:00Z
summary: "Schulman et al.'s TRPO: a policy-gradient method that bounds each update by a KL-divergence trust region, guaranteeing monotonic improvement on a surrogate objective"
title: "Trust Region Policy Optimization"
authors: [Schulman, Levine, Moritz, Jordan, Abbeel]
year: 2015
venue: ICML
arxiv_id: "1502.05477"
url: https://arxiv.org/abs/1502.05477
tags: [reinforcement-learning, policy-gradient, trust-region, trpo]
source: [[sources/papers/schulman-trpo-2015]]
concepts:
  - [[records/concepts/trust-region-methods]]
  - [[records/concepts/policy-gradient]]
  - [[records/concepts/actor-critic]]
---

# TRPO (Schulman et al. 2015)

Trust Region Policy Optimization. Vanilla policy gradients are fragile:
too large a step collapses the policy. TRPO constrains each update so the
new policy stays within a trust region of the old one, measured by KL
divergence, and proves this yields monotonic improvement of a surrogate
objective. The constrained optimization is solved with the conjugate
gradient method. Reliable but heavy; PPO later traded the hard constraint
for a cheap clip.

See [[records/concepts/trust-region-methods]] for the formulation and
[[records/papers/schulman-ppo-2017]] for the simplification.
