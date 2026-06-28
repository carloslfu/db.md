---
type: paper
id: schulman-ppo-2017
created: 2026-04-23T11:30:00Z
updated: 2026-05-23T14:30:00Z
summary: "Schulman et al.'s PPO: a first-order policy-gradient method whose clipped surrogate objective keeps updates near the old policy without TRPO's constrained solve"
title: "Proximal Policy Optimization Algorithms"
authors: [Schulman, Wolski, Dhariwal, Radford, Klimov]
year: 2017
venue: arXiv
arxiv_id: "1707.06347"
url: https://arxiv.org/abs/1707.06347
tags: [reinforcement-learning, policy-gradient, trust-region, ppo]
source: [[sources/papers/schulman-ppo-2017]]
concepts:
  - [[records/concepts/trust-region-methods]]
  - [[records/concepts/policy-gradient]]
  - [[records/concepts/actor-critic]]
---

# PPO (Schulman et al. 2017)

Proximal Policy Optimization. PPO keeps TRPO's "stay close to the old
policy" intuition but drops the second-order constrained solve. Its
clipped surrogate objective caps the probability ratio between new and old
policy, removing the incentive to step too far, and it optimizes that
objective with plain stochastic gradient ascent over several epochs per
batch. Simple, scalable, and the default policy-gradient algorithm in
practice today.

See [[records/concepts/trust-region-methods]] for how the clip relates to
TRPO's KL constraint, and [[records/concepts/actor-critic]] for the
value-baseline pairing.
