---
type: concept
meta-type: conclusion
id: q-learning
created: 2026-04-09T16:30:00Z
updated: 2026-05-23T15:05:00Z
summary: "Off-policy TD control that learns the optimal action-value function Q*(s,a) directly via a max over next-state actions; the algorithm DQN scaled to pixels"
topic: q-learning
tags: [reinforcement-learning, q-learning, off-policy, temporal-difference]
derived_from:
  - [[records/papers/watkins-qlearning-1992]]
  - [[records/papers/mnih-dqn-2015]]
---

# Q-learning

Q-learning is an off-policy temporal-difference control algorithm that
learns the optimal action-value function Q*(s, a) — the expected return of
taking action a in state s and acting optimally thereafter — directly from
experience, without a model of the environment.

## Mechanism

Q-learning is [[records/concepts/temporal-difference-learning]] applied to
the action-value function, with one crucial twist. The update is

    Q(s, a) <- Q(s, a) + alpha * [ r + gamma * max_{a'} Q(s', a') - Q(s, a) ]

The `max` over next-state actions is what makes the method **off-policy**:
the target assumes the *greedy* action will be taken next, regardless of
what the behaviour policy actually does. The agent can therefore explore
with one policy (visiting suboptimal actions) while learning about another
(the optimal one). Watkins & Dayan ([[records/papers/watkins-qlearning-1992]])
proved that, in the tabular case, Q converges to Q* with probability one
as long as every state-action pair is visited infinitely often and the
step sizes satisfy the Robbins-Monro conditions.

## Off-policy vs on-policy

The `max` distinguishes Q-learning from its on-policy sibling SARSA, whose
target uses the action actually taken, `Q(s', a')`. Off-policy learning is
powerful — it permits learning the optimal policy from data generated any
which way, including a replay buffer of old transitions — but it is one leg
of the **deadly triad** (see
[[records/concepts/temporal-difference-learning]]) and so is harder to
stabilize under function approximation.

## Exploration

Because Q-learning learns about the greedy policy regardless of behaviour,
it needs a *separate* exploration policy to guarantee coverage. The
standard choice is ε-greedy: act greedily with probability 1 − ε, act
randomly otherwise. See [[records/concepts/exploration-exploitation]] for
the dilemma this addresses.

## From tables to pixels

Tabular Q-learning needs one entry per state-action pair, hopeless for
large problems. DQN ([[records/papers/mnih-dqn-2015]]) replaced the table
with a deep [[records/concepts/value-network]] mapping pixels to Q-values,
and made the unstable combination work with two tricks:
[[records/concepts/experience-replay]] (decorrelate and reuse transitions)
and a periodically-frozen target network (stabilize the bootstrapped
target). This is the result that opened deep reinforcement learning; the
lineage continues through [[records/concepts/actor-critic]] methods on the
policy-gradient side and into AlphaGo / AlphaZero on the search side
([[records/concepts/mcts]]).

## Open questions

- *Maximization bias:* the `max` operator systematically overestimates
  values; Double Q-learning splits selection from evaluation to reduce it.
- *Sample efficiency:* even with replay, value-based deep RL is data
  hungry compared to model-based alternatives.

## Related concepts

- [[records/concepts/temporal-difference-learning]]
- [[records/concepts/exploration-exploitation]]
- [[records/concepts/experience-replay]]
- [[records/concepts/function-approximation]]
- [[records/concepts/value-network]]
- [[records/concepts/markov-decision-process]]
