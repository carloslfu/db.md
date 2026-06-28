---
type: concept
meta-type: conclusion
id: trust-region-methods
created: 2026-04-23T16:00:00Z
updated: 2026-05-23T15:35:00Z
summary: "Policy-gradient methods that bound how far each update moves the policy — TRPO via a KL trust region, PPO via a clipped ratio — to keep learning stable"
topic: trust-region-methods
tags: [reinforcement-learning, policy-gradient, trust-region, trpo, ppo]
derived_from:
  - [[records/papers/schulman-trpo-2015]]
  - [[records/papers/schulman-ppo-2017]]
---

# Trust-region methods

Trust-region methods are [[records/concepts/policy-gradient]] algorithms
that explicitly **limit how far each update is allowed to move the policy**.
They exist to solve the step-size fragility of vanilla policy gradients: in
RL the data distribution depends on the policy, so an over-large step can
push the policy into a region where the just-collected data is no longer
representative, and performance collapses with no easy way back.

## TRPO: a hard KL constraint

Trust Region Policy Optimization ([[records/papers/schulman-trpo-2015]])
maximizes a surrogate objective subject to a hard constraint that the KL
divergence between the new and old policy stays below a threshold δ:

    maximize  E[ (π_new / π_old) * A ]   subject to   KL(π_old, π_new) ≤ δ

Schulman et al. show this guarantees **monotonic improvement** of the true
objective under a principled approximation. The constrained problem is
solved with the conjugate-gradient method and a line search, which requires
a second-order computation (a Fisher-vector product). The result is
reliable but heavy to implement and run, and the advantage A comes from an
[[records/concepts/actor-critic]] critic.

## PPO: a cheap clip

Proximal Policy Optimization ([[records/papers/schulman-ppo-2017]]) keeps
TRPO's "stay near the old policy" intuition but replaces the constrained
second-order solve with a **first-order** surrogate. It clips the
probability ratio r = π_new / π_old to the interval [1 − ε, 1 + ε] and
takes the minimum of the clipped and unclipped objective:

    L = E[ min( r * A,  clip(r, 1 - ε, 1 + ε) * A ) ]

Once the ratio leaves the clip range, the objective flattens, so there is no
gradient incentive to step further. PPO optimizes this with ordinary SGD
over several epochs per batch of data. It is far simpler than TRPO,
parallelizes well, and is the default policy-gradient algorithm in practice
today — including as the workhorse of RLHF for language models.

## What they share

Both bound the update in policy space rather than parameter space, both are
[[records/concepts/actor-critic]] methods using an advantage signal, and
both are on-policy, so each batch is used and discarded — they do not reuse
a buffer the way [[records/concepts/experience-replay]] does for value-based
methods.

## Open questions

- *Constraint vs clip:* PPO's clip is a heuristic approximation of TRPO's
  principled KL constraint; exactly when the approximation is adequate is
  not fully characterized.
- *Hyperparameter sensitivity:* PPO's behaviour depends on the clip range,
  epoch count, and minibatch scheme more than its simplicity suggests.

## Related concepts

- [[records/concepts/policy-gradient]]
- [[records/concepts/actor-critic]]
- [[records/concepts/function-approximation]]
- [[records/concepts/exploration-exploitation]]
