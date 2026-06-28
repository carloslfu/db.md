---
type: concept
meta-type: conclusion
id: temporal-difference-learning
created: 2026-04-08T16:00:00Z
updated: 2026-05-23T15:00:00Z
summary: "Learning predictions from the difference between successive estimates (bootstrapping) instead of waiting for the final outcome; the engine under Q-learning, DQN, and actor-critic"
topic: temporal-difference-learning
tags: [reinforcement-learning, temporal-difference, prediction, foundations]
derived_from:
  - [[records/papers/sutton-td-1988]]
  - [[records/papers/tesauro-tdgammon-1995]]
---

# Temporal-difference learning

Temporal-difference (TD) learning is a family of methods that learn to
predict a long-run quantity — usually the return of a state — by updating
each estimate toward a *later* estimate of the same quantity, rather than
toward the final observed outcome. This act of learning a guess from a
guess is called **bootstrapping**, and it is the single idea that
separates TD methods from Monte Carlo methods.

## Mechanism

Consider estimating the value V(s), the expected return from state s.
Monte Carlo waits for the episode to end, then nudges V(s) toward the
actual return. TD does not wait. After one step it forms the **TD target**

    r + gamma * V(s')

— the immediate reward plus the discounted estimate of the next state —
and nudges V(s) toward that target by the **TD error**

    delta = r + gamma * V(s') - V(s).

The update is `V(s) <- V(s) + alpha * delta`. Because the target itself
contains a current estimate V(s'), the method bootstraps. This makes TD
**online** (it can learn from incomplete episodes) and usually **lower
variance** than Monte Carlo, at the cost of bias from the imperfect
estimate it bootstraps on. The bias-variance trade is exactly what the
eligibility-trace parameter λ tunes in TD(λ): λ = 0 is pure one-step TD,
λ = 1 recovers Monte Carlo, and intermediate values blend the two.

## History

Sutton named and formalized the family in
[[records/papers/sutton-td-1988]], proving convergence for the linear
prediction case and introducing TD(λ) with eligibility traces. The idea
had roots in Samuel's checkers player and in animal-learning models, but
Sutton's paper is the reference point.

The first dramatic empirical success was TD-Gammon
([[records/papers/tesauro-tdgammon-1995]]): Tesauro trained a backgammon
[[records/concepts/value-network]] by TD updates over
[[records/concepts/self-play]] games and reached world-class strength,
showing that bootstrapping a nonlinear approximator could work in
practice even before the theory caught up.

TD is the engine inside the control methods that followed.
[[records/concepts/q-learning]] is TD applied to the optimal
action-value function; DQN ([[records/papers/mnih-dqn-2015]]) scales that
to pixels; and the critic in [[records/concepts/actor-critic]] is trained
by a TD error.

## The deadly triad

Combining TD bootstrapping with [[records/concepts/function-approximation]]
and off-policy training is the **deadly triad**: each pair is benign, but
all three together can diverge. Much of deep RL is engineering around this
risk — DQN's target network and [[records/concepts/experience-replay]] are
two such mitigations.

## Open questions

- *Bias control:* the bootstrapped target is biased whenever the value
  estimate is wrong; n-step and λ-return methods manage this but do not
  eliminate it.
- *Stability under approximation:* convergence guarantees are weak once a
  nonlinear approximator enters the loop; the deadly triad has no general
  cure.

## Related concepts

- [[records/concepts/q-learning]]
- [[records/concepts/value-network]]
- [[records/concepts/function-approximation]]
- [[records/concepts/actor-critic]]
- [[records/concepts/markov-decision-process]]
