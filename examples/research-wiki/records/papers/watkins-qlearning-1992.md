---
type: paper
id: watkins-qlearning-1992
created: 2026-04-09T11:00:00Z
updated: 2026-05-23T14:05:00Z
summary: "Watkins & Dayan's Q-learning convergence proof: an off-policy TD control rule that learns the optimal action-value function regardless of the policy followed"
title: "Q-learning"
authors: [Watkins, Dayan]
year: 1992
venue: Machine Learning
doi: "10.1007/BF00992698"
url: https://link.springer.com/article/10.1007/BF00992698
tags: [reinforcement-learning, q-learning, off-policy, temporal-difference]
source: [[sources/papers/watkins-qlearning-1992]]
concepts:
  - [[records/concepts/q-learning]]
  - [[records/concepts/temporal-difference-learning]]
  - [[records/concepts/exploration-exploitation]]
---

# Q-learning (Watkins & Dayan 1992)

The paper that introduced Q-learning (Watkins 1989) and proved its
convergence (Watkins & Dayan 1992). Q-learning is an off-policy TD
control method: it learns the optimal action-value function Q*(s, a)
directly, independent of the behaviour policy generating the data, as
long as every state-action pair is visited infinitely often. The update
takes the max over next-state actions, which is what makes it off-policy.

See [[records/concepts/q-learning]] for the rule, and
[[records/concepts/exploration-exploitation]] for the behaviour-policy
requirement.
