---
type: concept
meta-type: conclusion
id: policy-gradient
created: 2026-04-10T16:00:00Z
updated: 2026-05-23T15:10:00Z
summary: "Optimizing a parameterized policy directly by ascending the gradient of expected return (REINFORCE); the basis for actor-critic, TRPO, and PPO"
topic: policy-gradient
tags: [reinforcement-learning, policy-gradient, reinforce, foundations]
derived_from:
  - [[records/papers/williams-reinforce-1992]]
  - [[records/papers/mnih-a3c-2016]]
---

# Policy gradient

Policy-gradient methods optimize a parameterized policy π_θ(a | s)
**directly**, by following the gradient of expected return with respect to
the parameters θ, instead of learning a value function and deriving a
policy from it. They are the natural choice when the action space is
continuous or when a stochastic policy is desirable.

## The policy-gradient theorem

The expected return J(θ) has a gradient that can be written without
differentiating the environment dynamics:

    ∇_θ J(θ) = E[ ∇_θ log π_θ(a | s) * Q^π(s, a) ]

This **likelihood-ratio** form is the basis of every policy-gradient
algorithm. Intuitively, it pushes up the log-probability of actions that
led to high return and pushes down those that led to low return — weighting
each by how good the action was.

## REINFORCE and variance

Williams ([[records/papers/williams-reinforce-1992]]) introduced the
REINFORCE estimator, which plugs the Monte Carlo return in for Q^π. It is
**unbiased** but **high variance**, because a full-episode return is a
noisy credit signal. Two standard fixes reduce the variance without adding
bias:

1. **Baselines.** Subtracting a state-dependent baseline b(s) from the
   return leaves the gradient unbiased but shrinks its variance. The
   value function V(s) is the canonical baseline.
2. **Bootstrapped critics.** Replacing the Monte Carlo return with a
   bootstrapped estimate from a learned critic gives the **advantage**
   A(s, a) = Q(s, a) − V(s). This is the
   [[records/concepts/actor-critic]] architecture, and the critic is
   trained by [[records/concepts/temporal-difference-learning]].

## On-policy by nature

Policy gradients are intrinsically **on-policy**: the expectation is taken
under the current policy, so each update needs fresh data from that policy.
This is why A3C ([[records/papers/mnih-a3c-2016]]) reaches for parallel
actors rather than a replay buffer — it decorrelates on-policy data instead
of reusing off-policy data the way DQN does with
[[records/concepts/experience-replay]].

## The step-size problem

A raw policy gradient is fragile: too large a step can collapse the policy
into a degenerate one from which it never recovers, because the data
distribution shifts with the policy. Controlling the size of each update is
the entire motivation for [[records/concepts/trust-region-methods]] —
TRPO's KL constraint ([[records/papers/schulman-trpo-2015]]) and PPO's clip
([[records/papers/schulman-ppo-2017]]) are two answers to the same problem.

## History

The likelihood-ratio idea predates RL, but Williams' REINFORCE is the RL
reference point. AlphaGo's RL phase ([[records/papers/silver-alphago-2016]])
refined its policy network by policy-gradient self-play before the value
network and [[records/concepts/mcts]] were layered on.

## Open questions

- *Variance at scale:* even with advantage baselines, gradient estimates
  are noisy in high-dimensional or sparse-reward settings.
- *Sample efficiency:* on-policy methods discard data after each update,
  making them less sample-efficient than off-policy value methods.

## Related concepts

- [[records/concepts/actor-critic]]
- [[records/concepts/trust-region-methods]]
- [[records/concepts/exploration-exploitation]]
- [[records/concepts/function-approximation]]
- [[records/concepts/markov-decision-process]]
