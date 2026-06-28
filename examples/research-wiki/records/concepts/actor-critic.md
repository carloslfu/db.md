---
type: concept
meta-type: conclusion
id: actor-critic
created: 2026-04-16T17:00:00Z
updated: 2026-05-23T15:15:00Z
summary: "Architecture pairing a policy (actor) with a learned value function (critic) that supplies a low-variance advantage signal; A3C scaled it with parallel workers"
topic: actor-critic
tags: [reinforcement-learning, actor-critic, policy-gradient, a3c]
derived_from:
  - [[records/papers/mnih-a3c-2016]]
  - [[records/papers/williams-reinforce-1992]]
---

# Actor-critic

Actor-critic methods sit between pure value-based and pure policy-based
reinforcement learning. They keep an explicit parameterized policy — the
**actor** — and pair it with a learned value function — the **critic** —
whose job is to supply a low-variance training signal for the actor.

## Mechanism

The actor is a [[records/concepts/policy-gradient]] learner: it updates
π_θ(a | s) along the likelihood-ratio gradient. The problem with plain
REINFORCE ([[records/papers/williams-reinforce-1992]]) is that the Monte
Carlo return used to weight each action is high variance. The critic fixes
this by estimating a value baseline and turning the weight into an
**advantage**:

    A(s, a) = Q(s, a) - V(s)  ≈  r + gamma * V(s') - V(s)

The right-hand approximation is exactly a TD error (see
[[records/concepts/temporal-difference-learning]]), so the critic is
trained by bootstrapping while the actor is trained by the advantage it
produces. The two learn together: a better critic gives the actor a
cleaner gradient; a better actor gives the critic a more relevant state
distribution.

## Why it matters

Actor-critic combines the strengths of both sides. From the policy side it
inherits the ability to represent stochastic policies and handle
continuous actions; from the value side it inherits the low-variance,
online credit assignment of TD learning. Nearly every modern
policy-gradient algorithm — A3C, A2C, TRPO, PPO, SAC — is an actor-critic
in this sense.

## A3C and parallelism

A3C ([[records/papers/mnih-a3c-2016]]) is the deep-RL result that made
actor-critic a default. Rather than stabilizing learning with a replay
buffer the way DQN does ([[records/concepts/experience-replay]]), A3C runs
many actor-learners in parallel, each on an independent copy of the
environment, and lets the diversity of their simultaneous experience
decorrelate the on-policy updates. The "advantage" in the name is the
critic-baselined gradient above. A2C is the synchronous variant that
reproduces A3C's gains without the asynchrony.

## Relationship to trust-region methods

The actor's update is still a policy gradient, and so it still has the
step-size fragility that [[records/concepts/trust-region-methods]] exist to
control. TRPO ([[records/papers/schulman-trpo-2015]]) and PPO
([[records/papers/schulman-ppo-2017]]) are actor-critic algorithms with a
constrained or clipped actor update; the critic underneath is unchanged.

## Open questions

- *Critic bias:* a poorly fit critic biases the actor's gradient; balancing
  critic accuracy against actor progress is delicate.
- *Two learning rates:* actor and critic learn at different speeds, and the
  interaction can be unstable without careful tuning.

## Related concepts

- [[records/concepts/policy-gradient]]
- [[records/concepts/temporal-difference-learning]]
- [[records/concepts/trust-region-methods]]
- [[records/concepts/value-network]]
- [[records/concepts/function-approximation]]
