# salsa-spatial codec design

salsa-spatial represents an object scene with a 5.1(side) compatible core,
two auxiliary height observations, compact covariance parameters, and a sparse
rank-r correction of the reconstruction matrix.

The public container is a standard 7.1 Ogg Opus mapping-family-1 track. Its
FL, FR, FC, LFE, SL, and SR channels carry the 5.1(side) fallback mix, BL and
BR are silent, and the two auxiliary observations travel in hidden coded
channels.

The decoder derives an object synthesis matrix from those parameters and the
known downmix. Object identities remain stable. The synthesis matrix can
change with time and frequency without assigning objects to broadband clusters.

The [implementation guide](salsa-spatial-implementation.md) contains the
concrete profile, transport, software structure, and integration behavior.
The [quality evaluation](salsa-spatial-quality.md) contains measured results.

## Contents

- [Signal model](#1-signal-model)
- [Clustering, estimation, and observation rank](#2-why-the-representation-avoids-hard-clustering)
- [Shared spatial downmix](#3-shared-spatial-downmix)
- [Covariance-driven reconstruction](#4-covariance-driven-reconstruction)
- [Time-frequency analysis and synthesis](#5-time-frequency-analysis-and-synthesis)
- [Parameter compression](#6-parameter-compression)
- [Closed-loop rate-distortion optimization](#7-closed-loop-rate-distortion-optimization)
- [Encoder and decoder procedures](#8-encoder-and-decoder-procedures)
- [Computational structure](#9-computational-structure)

## 1. Signal model

For one time-frequency region, use the following notation:

| Symbol | Meaning | Dimensions |
|---|---|---|
| $s$ | Source object spectra after authored gain and activity | $N\times1$ |
| $D$ | Downmix matrix derived from the transmitted positions | $J\times N$ |
| $x$ | Ideal full-range core spectra | $J\times1$ |
| $y$ | Locally decoded full-range core spectra | $J\times1$ |
| $M$ | Object synthesis matrix | $N\times J$ |
| $A_r$ | Object-to-output matrix for render target $r$ | $L_r\times N$ |
| $R_{ab}$ | Second moment $\mathbb E[ab^H]$ | Determined by $a,b$ |

Here, $J=7$. The superscript $H$ denotes the conjugate transpose. The LFE
signal bypasses the object model and is coded independently.

In the sample domain, the downmix is

$$
x[n]=D[n]s[n].
$$

A shared headroom gain scales all signals together. The following equations
use a common signal level with that gain factored out.

Within a time-frequency region, the model freezes the downmix at the frame
center:

$$
x(t,k)\approx D(t)s(t,k),\qquad
y(t,k)=x(t,k)+e(t,k),\qquad
\hat s(t,k)=M(t,b(k))y(t,k).
$$

The approximation comes from motion within the analysis window. The encoder
measures the actual sample-domain downmix and decoded core. Those observations
are used when it evaluates candidate parameters.

~~~mermaid
flowchart LR
    S["Source objects and trajectories"] --> D["Shared downmix"]
    D --> C["Audio core encode and local decode"]
    S --> E["Source and decoded-core statistics"]
    C --> E
    E --> P["Quantized parameter search"]
    P --> T["Parameter prediction and entropy coding"]
    C --> Y["Decoded core"]
    T --> V["Reconstructed covariance, trajectories, and correction"]
    V --> M["Regularized synthesis matrix"]
    Y --> M
    M --> O["Reconstructed object signals"]
    O --> R["Speaker or binaural render"]
~~~

The core supplies waveform information. The parameters control how the
decoder distributes that information among the objects, and the sparse
reconstruction correction removes part of the residual mapping error that the
pair covariance cannot represent.

## 2. Why the representation avoids hard clustering

### 2.1 Linear minimum-error synthesis

Consider all linear estimators $\hat s=My$ for a fixed decoded core $y$.
Their mean-square error is

$$
\mathcal E(M)
=\operatorname{tr}(R_{ss})
-2\operatorname{Re}\operatorname{tr}(MR_{ys})
+\operatorname{tr}(MR_{yy}M^H).
$$

The error is orthogonal to the observations at an optimum:

$$
\mathbb E[(s-M_\star y)y^H]=0,
\qquad M_\star R_{yy}=R_{sy}.
$$

A linear minimum mean-square error (LMMSE) solution is therefore

$$
M_\star=R_{sy}R_{yy}^{\dagger},
$$

where $\dagger$ is the Moore–Penrose pseudoinverse. When $R_{yy}$ is
positive definite, the ordinary inverse gives the unique solution.

For any other matrix $M$,

$$
\mathcal E(M)-\mathcal E(M_\star)
=\operatorname{tr}\left[
(M-M_\star)R_{yy}(M-M_\star)^H
\right]\geq0.
$$

This identity also gives the equality condition: both matrices must act
identically on the supported observation subspace.

### 2.2 The restriction introduced by a cluster map

Let a broadband expansion matrix $U\in\mathbb C^{N\times K}$ map $K$
cluster signals back to objects. A clustered synthesis has the form

$$
M_{\mathrm{cluster}}=UB,\qquad B\in\mathbb C^{K\times J}.
$$

Every column of $UB$ lies in the column space of $U$. If $U$ has rank $r$,
this family has at most $rJ$ complex dimensions, compared with $NJ$ for
an unrestricted synthesis matrix. Thus, for the same source and decoded core,

$$
\min_M\mathcal E(M)\leq\min_B\mathcal E(UB).
$$

The inequality concerns synthesis from the same observations. Changing the
downmix or the core codec changes the observations and therefore the
optimization problem.

salsa-spatial removes the fixed expansion matrix $U$. It represents $M$
through a compact covariance model instead. That model is a restricted
subset of all synthesis matrices; the encoder searches within that subset.

### 2.3 What happens to two indistinguishable objects

Suppose two uncorrelated objects have the same downmix column $d$, with
powers $\sigma_1^2$ and $\sigma_2^2$. In the noiseless case,

$$
x=d(s_1+s_2).
$$

The unregularized LMMSE estimates are

$$
\hat s_1=\alpha_1(s_1+s_2),\qquad
\hat s_2=\alpha_2(s_1+s_2),\qquad
\alpha_i=\frac{\sigma_i^2}{\sigma_1^2+\sigma_2^2}.
$$

Each object's residual error power is
$\sigma_1^2\sigma_2^2/(\sigma_1^2+\sigma_2^2)$. The core contains
the sum, so the covariance assigns that sum according to the local powers.
The assignment can differ in every time-frequency region.

Let $a_1$ and $a_2$ be the destination renderer's columns for the two objects.
The reconstructed render is exactly

$$
\hat z=(\alpha_1a_1+\alpha_2a_2)(s_1+s_2).
$$

If $a_1=a_2$, the rendered sum is exact even though the separated objects
contain error. For nearby positions $\mathbf p_1,\mathbf p_2$ and a locally
affine panner,

$$
a_{\mathrm{eff}}=\alpha_1a(\mathbf p_1)+\alpha_2a(\mathbf p_2)
\approx a(\alpha_1\mathbf p_1+\alpha_2\mathbf p_2).
$$

The weighted renderer column is exact; the weighted position is a local
approximation. This distinction matters for nonlinear speaker panners and
frequency-dependent binaural filters.

### 2.4 Observation rank and height

Every synthesis matrix has $\operatorname{rank}(M)\leq J$. Additional object
outputs do not create additional independent observations. At each
time-frequency point, all reconstructed signals remain combinations of the
seven core signals.

Height channels add directions to the observation space. Two positions can
have nearly identical floor-channel gains but different height gains.
Retaining the height pair lets the covariance model use that difference.
An object component in the null space of the full downmix still has no
waveform observation.

## 3. Shared spatial downmix

### 3.1 Panning law

Positions use normalized coordinates $(x,y,z)$: right, forward, and up.
Define

$$
u=\operatorname{clamp}((x+1)/2,0,1),\qquad
v=\operatorname{clamp}(1-y,0,1),\qquad
h=\operatorname{clamp}(z,0,1),
$$

and the equal-power pair

$$
P(a)=
\begin{bmatrix}
\cos(\pi a/2)\\
\sin(\pi a/2)
\end{bmatrix}.
$$

In channel order $(L,R,C)$, the front vector is

$$
F(u)=
\begin{cases}
[\cos(\pi u),\,0,\,\sin(\pi u)]^T,&u\leq1/2,\\
[0,\,\sin(\pi(u-1/2)),\,\cos(\pi(u-1/2))]^T,&u>1/2.
\end{cases}
$$

Let $\rho\in[0,1)$ be the horizontal gain retained at the ceiling. The
default is zero. The vertical coding angle is

$$
\theta_\rho(h)=h\arccos\rho.
$$

The floor and full-range columns are

$$
f=
\begin{bmatrix}
\cos(\pi v/2)F(u)\\
\sin(\pi v/2)P(u)
\end{bmatrix},
\qquad
d=
\begin{bmatrix}
\cos\theta_\rho(h)f\\
\sin\theta_\rho(h)P(u)
\end{bmatrix}.
$$

The resulting order is $(L,R,C,L_s,R_s,H_L,H_R)$. At a panner evaluation
point, $\|d\|_2^2=1$. This controls the contribution of one object. Correlated
objects can still add coherently and require programme headroom.

For $\rho=0$, the original vertical curve is unchanged. For positive
$\rho$, the ceiling retains horizontal gain $\rho$ and height gain
$\sqrt{1-\rho^2}$. Front/back columns therefore remain distinct at maximum
height. The vertical derivative of the geometric column has norm
$\arccos\rho$ at fixed $(u,v)$, including the ceiling. The coding angle does
not change authored positions or target-render panning. A higher matrix
rank alone does not establish a gain after audio coding.

The transport applies the fixed fallback fold to these geometric rows:
$d_{\rm obs}=[g+A_0h;h]$. The local RD search and decoder both use
$d_{\rm obs}$, including the fold, as their reconstruction column. The
headroom scan measures the actual folded signals. Unit geometric-column
energy does not imply unit folded-column energy.

### 3.2 Trajectory coding and shared gains

The encoder first quantizes and fits the object trajectories. It then forms
the downmix from those reconstructed trajectories. The decoder derives $D$
from the same positions.

The trajectory fit controls gain error, since equal coordinate errors can
produce different render errors. For each validation panner $a_r$, the
accepted segment satisfies a bound of the form

$$
\|a_r(p(t))-a_r(\tilde p(t))\|_2\leq\varepsilon_{\mathrm{pos}}
$$

at the evaluated control points. Authored steps remain at their sample times.
Linear segments describe continuous motion.

Panner gains are evaluated on a shared control grid and interpolated between
control points. An interpolated column can have a norm below one. Both sides
use that column as it is; renormalizing it on only one side would change the
reconstruction model.

## 4. Covariance-driven reconstruction

### 4.1 From the Wiener estimator to transmitted parameters

Under the model $y=Ds+e$ with $R_{se}=0$,

$$
R_{sy}=R_{ss}D^H,\qquad
R_{yy}=DR_{ss}D^H+R_{ee}.
$$

Substitution into the LMMSE solution gives

$$
M=R_{ss}D^H(DR_{ss}D^H+R_{ee})^{-1}.
$$

This form replaces transmission of $NJ$ complex matrix entries with a model
of source covariance and core error. The downmix is already known from the
trajectories.

Core coding error can depend on the source. The encoder therefore evaluates
this reconstruction family with measured $R_{sy}$ and $R_{yy}$. The
uncorrelated-error model defines the synthesis form; the measured moments
define the optimization loss.

### 4.2 Relative powers and scale invariance

Let $\mathcal A$ be the active object set in a region. Relative powers are

$$
q_i=
\begin{cases}
\displaystyle\frac{\exp(a\ell_i)}
{\sum_{j\in\mathcal A}\exp(a\ell_j)},&i\in\mathcal A,\\
0,&i\notin\mathcal A,
\end{cases}
\qquad a=\frac{\ln10}{10}.
$$

The log powers $\ell_i$ are measured in dB. A common offset cancels from $q$.
The encoder fixes this freedom by setting the largest active log power in
each band to zero. An inactive object has exactly zero power and retains its
object identity.

For a diagonal source model, $C=\operatorname{diag}(q)$. Absolute scene
power is not needed: multiplying both the covariance and its associated
regularizer by the same positive scalar leaves $M$ unchanged.

### 4.3 Sparse correlation pairs

A diagonal model treats source objects as uncorrelated. The selected model
adds disjoint object pairs to represent shared signals and phase relations:

$$
C=Q^{1/2}HQ^{1/2},\qquad Q=\operatorname{diag}(q).
$$

The diagonal of $H$ is one. For a selected pair $(i,j)$,

$$
H_{[i,j],[i,j]}=
\begin{bmatrix}
1&\rho_{ij}\\
\rho_{ij}^{*}&1
\end{bmatrix},
\qquad
C_{ij}=\sqrt{q_iq_j}\rho_{ij}.
$$

The pair block has eigenvalues $1\pm|\rho_{ij}|$. Bounding
$|\rho_{ij}|<1$ and making the pairs disjoint keeps $H$ positive definite.
Zero inactive powers make the full $C$ positive semidefinite.

The pair list and its per-band coefficients are fixed within a parameter
tile. Powers vary at the tile's anchors. This separates slower correlation
structure from faster energy changes. Pair selection uses rendered
rate-distortion cost, so a large measured correlation alone does not force
transmission of a pair.

### 4.4 Core error and regularization

The encoder measures error against the actual ideal core:

$$
\nu_{k,b}
=\frac{\mathbb E|y_{k,b}-x_{k,b}|^2}
{\max(\mathbb E\sum_{j=1}^{J}|x_{j,b}|^2,\epsilon)}.
$$

The denominator is total ideal **core** power in the band. It is not the
sum of source object powers: coherent addition or cancellation changes
core power.

Define

$$
Z=DCD^H,\qquad
E=\operatorname{tr}(Z)\operatorname{diag}(\nu),\qquad
K_0=Z+E.
$$

The trace factor puts source and error covariance on the same scale. Under
the frozen-downmix model, let $P_o=\operatorname{tr}(R_{ss})$ and
$P_c=\operatorname{tr}(DR_{ss}D^H)$. For $C=R_{ss}/P_o$,

$$
\operatorname{tr}(Z)=\frac{P_c}{P_o},\qquad
\operatorname{tr}(Z)\nu_k
=\frac{P_c}{P_o}\frac{(R_{ee})_{kk}}{P_c}
=\frac{(R_{ee})_{kk}}{P_o}.
$$

Thus, the error term is normalized by the same source power as $C$.
Only channelwise core-error powers are transmitted. The measured RD loss
also contains cross-channel error effects through $R_{sy}$ and $R_{yy}$.

The measured $\nu$ alone reduces amplification of noisy core channels. It is
separated from numerical stabilization. Let

$$
S=\operatorname{diag}(K_0)^{-1/2},\qquad
R=SK_0S,
$$

with $S$ restricted to the supported observation channels, where the diagonal
of $K_0$ is positive. The decoder uses

$$
M=CD^HS(R+\epsilon I)^{-1}S,\qquad \hat s=My.
$$

$S$ removes channel scale differences. The dimensionless floor $\epsilon$
only stabilizes the normalized solve; it is not a signal-level regularization
parameter. In unnormalized coordinates the same system is

$$
K=K_0+\epsilon\operatorname{diag}(K_0),\qquad
M=CD^HK^{-1}.
$$

The normalized floor scales with each direction's own diagonal, so
independent weak and strong directions do not share a common extra floor. It
is about $10^{-12}$, far below any transmitted signal level. Channels with an
exactly zero diagonal are excluded from the supported subspace and contribute
no estimate; they do not turn active objects into inactive ones.

The solve remains $J\times J$ regardless of the object count. Factoring
$R+\epsilon I$ once gives both $M$ and the inverse actions used by the RD
derivatives.

If the source contributes negligible ideal core energy in a band, the
encoder marks that band unobservable. The decoder emits zero for that band.
This also handles source cancellation without assigning codec noise to
active objects.

### 4.5 Covariance interpolation

Between two anchors at times $t_0,t_1$, let
$\alpha=(t-t_0)/(t_1-t_0)$. Interpolate the covariance itself:

$$
C(t)=(1-\alpha)C_0+\alpha C_1.
$$

For any vector $v$,

$$
v^HC(t)v=(1-\alpha)v^HC_0v+\alpha v^HC_1v\geq0.
$$

Thus, interpolation preserves positive semidefiniteness even when the
adjacent anchors have different pair sets. The interpolated representation
retains the union of their pair terms. An object can temporarily have two
partners during this interpolation.

Core error powers are interpolated in linear power units. The decoder then
forms a new $K$ and $M$ at the hop. Interpolating log powers or correlation
phases separately would define a different covariance path.

Reconstruction-matrix corrections follow the same rule at their own
keyframes. Between two correction keyframes, the dense $\Delta M$ is
interpolated linearly. An empty correction keyframe is represented by zero, so
a correction can fade out within a corrected tile. A tile with no correction at all leaves the surrounding keyframes to
interpolate across it.
Either way, the pair baseline and its regularization are unchanged.

### 4.6 Reconstruction-matrix correction

The pair covariance supplies the baseline estimator $M_0$ from Section 4.4.
It is a sparse approximation to a dense, common-factor source covariance. The
encoder also measures the empirical linear estimator over a region:

$$
M_\star=R_{sy}R_{yy}^{\dagger}.
$$

For a fixed correction $\Delta M$ applied to a region containing hops $h$,
write $M_h=M_{0,h}+\Delta M$ and define the moment-weighted baseline

$$
\bar M_0=
\left(\sum_h M_{0,h}R_{yy,h}\right)
\left(\sum_h R_{yy,h}\right)^{\dagger}.
$$

Let $R_{yy}=\sum_h R_{yy,h}$ and $R_{sy}=\sum_h R_{sy,h}$. The missing
mapping and its weighted form are

$$
D=M_\star-\bar M_0,\qquad G=DR_{yy}^{1/2}.
$$

The rank-$r$ correction that minimizes the measured residual is

$$
\Delta M_r=G_rR_{yy}^{\dagger/2},
\qquad
G=U\Sigma V^H,
\qquad
G_r=U_r\Sigma_rV_r^H.
$$

Equivalently,

$$
\Delta M_r=\sum_{k=1}^{r}u_kw_k^H,
$$

where $u_k$ is a sparse complex left factor over source objects and $w_k^H$
is a dense seven-channel right row. This is a correction to the mapping from
the seven core observations, not a replacement source covariance. The pair
model and its sparse rate advantage remain the baseline.

The encoder fits each component as follows:

1. Select the objects with the largest left-factor energy, restricted to
   objects that are active in the region.
2. Quantize the left factor with a per-component scale.
3. Solve the least-squares right row for the **quantized** left factor,
   $w^H=u^HD/(u^Hu)$. For an invertible $R_{yy}$ this does not depend on the
   metric weighting; the pseudo-inverse handles rank deficiency.
4. Repeat greedily on the residual $D-\sum_{k<r}\Delta M_k$, so each component
   removes error left by the preceding quantized components.
5. Keep a component only when its measured distortion reduction exceeds a
   Lagrangian rate weight times its signaling bytes.

The profile bounds the rank per band and the sparse support. The encoder
selects the rank, support, keyframe density, and rate-distortion tradeoff with
the same cost used for the other parameters.

A tile carries a small number of correction keyframes, evenly spaced over the
tile. The decoder interpolates the dense correction between keyframes with the
same convex rule used for covariance:

$$
\Delta M(t)=(1-\alpha)\Delta M_0+\alpha\Delta M_1.
$$

Interpolating the correction keeps the baseline $M_0$ and its regularization
untouched. Correction keyframes reset at tile boundaries and do not enter the
persistent power, pair, or trajectory predictors.

The correction is an optional side parameter set for a tile. A decoder that
omits it uses the pair estimator $M_0$. The syntax bounds rank and support, so
the correction remains a sparse correction to the mapping rather than a dense
object covariance.

Selection uses the measured deterministic moments, not the Gaussian formula.
Activity gates still apply: inactive objects have exactly zero power and are
excluded from the support, so a correction cannot inject energy into a silent
object.

## 5. Time-frequency analysis and synthesis

### 5.1 Analysis frame and parameter bands

Let $L$ be the transform length and $H$ the hop. Use the square-root Hann
window

$$
w[n]=\sqrt{\frac12-\frac12\cos(2\pi n/L)},\qquad 0\leq n<L.
$$

Frame $t$ is centered at source sample $tH$:

$$
X(t,k)=\sum_{n=0}^{L-1}
w[n]x[tH+n-L/2]e^{-j2\pi kn/L}.
$$

The source, ideal core, and decoded core use the same frame centers and
windows. Samples outside the programme are zero. Tile boundaries do not
reset the analysis window.

Positive-frequency bins are grouped into parameter bands. One matrix
$M(t,b)$ applies to all bins in band $b$. Narrower low-frequency bands give
the model more frequency resolution where a single broad covariance would
mix distinct components.

For moment accumulation over positive-frequency bins, use weight one at DC
and Nyquist and weight two at interior bins. For example,

$$
\widehat R_{sy}(t,b)=
\sum_{k\in\mathcal K_b}\omega_k s(t,k)y(t,k)^H.
$$

The same weights apply to source power, decoded-core covariance, and error
power. These are energy sums; consistent normalization cancels in the
relative-power and error-envelope ratios.

### 5.2 Real endpoints and overlap-add

Real signals have real DC and Nyquist coefficients. At those bins, use
$\operatorname{Re}(C)$ and a real observation covariance. The other positive
bins use the complex model, with conjugate symmetry supplied for synthesis.

Let $v_{i,t}$ be the inverse transform of the reconstructed object spectrum,
and let $m_t=n-tH+L/2$. The synthesized object is

$$
\hat s_i[n]=
\frac{\sum_t w[m_t]v_{i,t}[m_t]}
{\sum_t w[m_t]^2},
$$

where only windows containing sample $n$ contribute. The denominator keeps
the gain correct at programme boundaries as well as in the steady state.

### 5.3 Three distinct time scales

| Time scale | Role |
|---|---|
| Audio samples | Downmix gains, authored position steps, and output PCM |
| Analysis hops | Reconstruction matrices and their covariance interpolation |
| Parameter anchors and tiles | Transmitted model updates and prediction state |
| Correction keyframes | Sub-tile updates of the synthesis correction, interpolated at each hop |

The analysis window controls spectral resolution. Anchor spacing controls
how quickly the model can change. Additional anchors around transients
reduce interpolation error without changing the transform.

## 6. Parameter compression

The metadata rate is reduced before entropy coding: the model removes
absolute power, predicts every active band from its previous reconstructed
value, omits inactive objects, and entropy codes the optional reconstruction
correction with the same integer probability machinery.

### 6.1 Per-band temporal power prediction

For object $i$, band $b$, and anchor $t$, the selected quantized log power is
$\ell_i(t,b)$ on the fixed half-dB grid. Relative powers in each band are
normalised with the band maximum set to zero. An inactive object has exactly
zero power and emits no power symbols.

For an active object, the encoder codes

$$
\Delta_i(t,b)=\ell_i(t,b)-\ell_i(t-1,b),
$$

where $\ell_i(t-1,b)$ is the previous **reconstructed** anchor value. The
predictor is initialised to zero at the start of an independent checkpoint
and whenever an object is inactive. Reactivation therefore codes its target
from zero.

Power deltas use one adaptive model per band. Prediction is purely temporal:
each active band refers to its previous reconstructed value.

### 6.2 Quantization and exact predictor state

The encoder selects a quantized target log-power vector and codes each active
band as an integer temporal delta. Coarser quantizers select subsets of the
fixed half-dB grid, so changing the quantizer scale does not reinterpret the
stored predictor state.

Each coded delta is the difference from the previous **reconstructed** value.
Both sides update the predictor with the reconstructed value. This closed
prediction loop prevents quantization error from causing state drift. The
representation reproduces the selected quantized target exactly; loss comes
from selecting that target, its temporal sampling, and the covariance model.

### 6.3 Other parameter families

| Parameters | Representation |
|---|---|
| Object powers | Per-band temporal deltas from the previous reconstructed anchor; inactive objects emit nothing |
| Object activity | Explicit state changes |
| Correlation pairs | Object indices, per-band magnitude, and wrapped phase innovations |
| Reconstruction correction | Entropy-coded rank, sparse object support, quantized complex factors, right rows, and per-component scales |
| Core error | Two-dimensional logarithmic prediction and unobservable flags |
| Trajectories | Quantized piecewise-linear positions and sample-time updates |
| Recovery state | Periodic independent predictor and trajectory checkpoints |

The activity state can suppress a negligible object contribution when the
RD reduction justifies it. Suppression changes the estimated signal, not
the object's identity or its membership in the scene.

### 6.4 Adaptive rANS

Symbols use separate adaptive integer probability models for each parameter
family. Every legal symbol has nonzero frequency. Deterministic rescaling
bounds the counts and lets the models adapt to recent symbols.

For total frequency $F$, symbol frequency $f_s$, cumulative frequency $c_s$,
and coder state $v$, the rANS state update is

$$
v'=\left\lfloor\frac{v}{f_s}\right\rfloor F
+(v\bmod f_s)+c_s.
$$

The decoder finds the symbol whose interval contains $u=v'\bmod F$ and
recovers

$$
v=f_s\left\lfloor\frac{v'}F\right\rfloor+(u-c_s).
$$

Renormalization transfers words between the state and the bitstream.
The encoder obtains adaptive intervals in decoding order, then encodes
them in reverse order. The decoder consumes symbols in forward order
and performs the same probability updates.

Entropy contexts persist across parameter tiles and reset only at independent
checkpoints. Parameter predictors likewise continue across tiles. A checkpoint
has its own entropy stream and records the state immediately before its tile,
so recovery does not depend on earlier tiles. The rate proxy uses the same
integer frequencies as the coder:

$$
R(\theta)=\sum_m-\log_2\frac{f_m}{F}+R_{\mathrm{overhead}}.
$$

This cost can rank parameter choices without introducing a separate,
inconsistent probability model.

The optional correction trailer uses an independent set of adaptive integer
models for its keyframe structure, support gaps, complex factor nibbles, and
scale fields. It is decoded from its own length-delimited trailer, so a
decoder that omits the correction still reconstructs the pair baseline.

## 7. Closed-loop rate-distortion optimization

### 7.1 Optimize the decoded render

The encoder first encodes and locally decodes the audio core. It optimizes
parameters against that decoded core, so the search sees core coding error.
Measured powers initialize the search. Optimized powers are model parameters
and can differ from measured powers when that reduces the decoded RD cost.

For render target $r$, let

$$
z_r=A_{r,\mathrm{ref}}s,\qquad
\hat z_r=A_{r,\mathrm{dec}}M_\theta y.
$$

The reference matrix uses source trajectories. The candidate matrix uses
transmitted trajectories. This includes trajectory error in the objective.

Over a tile, define reference energies $P_r$ and $P_s$ using the same
time-frequency samples as the loss. With a small denominator floor, use

$$
\mathcal D(\theta)=
\sum_r\frac{w_r}{P_r}
\sum_{t,b}\mathbb E\|z_r-\hat z_r\|_2^2
+\frac{w_s}{P_s}
\sum_{t,b}\mathbb E\|s-M_\theta y\|_2^2,
$$

$$
J(\theta)=\mathcal D(\theta)+\beta R(\theta).
$$

The selected render targets are the core panner, 7.1.4, and 9.1.6, with
equal weights summing to one. The object term has weight $w_s=0.1$.
It penalizes object errors that the selected render matrices may cancel.

Energy normalization prevents one loud render target from dominating the
others. It does not remove frequency weighting by signal energy within a
target. Weak spectral details can therefore contribute little to this loss.

The weight $\beta$ is fixed for an encode. It expresses how much distortion
reduction justifies an additional parameter bit. Core bitrate settings are
independent of $\beta$.

### 7.2 Evaluate candidates from measured moments

Let $B_r=A_{r,\mathrm{dec}}M_\theta$. For one region,

$$
\mathbb E\|z_r-B_ry\|_2^2
=\operatorname{tr}(R_{z_rz_r})
-2\operatorname{Re}\operatorname{tr}(B_rR_{yz_r})
+\operatorname{tr}(B_rR_{yy}B_r^H).
$$

The encoder measures $R_{sy}$, $R_{yy}$, and the source cross moments.
It obtains $R_{z_ry}=A_{r,\mathrm{ref}}R_{sy}$ and the reference render
power from those measurements.

The measured source moments retain correlations beyond the sparse decoder
model. Using the candidate covariance as the reference covariance would
make the loss reward its own assumptions.

The moment form gives the same quadratic loss as summing the modeled
spectral residuals. It avoids reconstructing an entire waveform for every
candidate. Matrices and moments are evaluated on the hop grid, so every hop
affected by an anchor contributes to that anchor's decision.

### 7.3 Differentiate through reconstruction

Continuous power refinement uses the dependence of $M$ on $C$. Hold $D$
and the measured error envelope fixed during a power step. Let
$dZ=D(dC)D^H$ and let $\operatorname{diag}(\cdot)$ keep only the diagonal.
Then

$$
dK=dZ+\epsilon\operatorname{diag}(dZ)
+(1+\epsilon)\operatorname{tr}(dZ)\operatorname{diag}(\nu),
$$

$$
dM=(dC)D^HK^{-1}-M(dK)K^{-1}.
$$

The second term includes both the covariance inverse and the change in the
trace-scaled error envelope and the diagonal numerical floor. Dropping any
contribution changes the gradient.

For active log power $\ell_j$, let $E_j$ be a diagonal matrix with a one
at position $j$. With the correlation matrix held fixed,

$$
\frac{\partial C}{\partial\ell_j}
=\frac a2(E_jC+CE_j)-a q_jC,\qquad a=\frac{\ln10}{10}.
$$

The last term is the derivative of power normalization. A common shift of
all active log powers must leave $C$ unchanged.

For a region, write the distortion differential as

$$
d\mathcal D=2\operatorname{Re}\operatorname{tr}(G^HdM),
$$

where $G$ combines the weighted render residuals and the object residual:

$$
G=
\sum_r\frac{w_r}{P_r}
A_{r,\mathrm{dec}}^H
\left(A_{r,\mathrm{dec}}MR_{yy}-R_{z_ry}\right)
+\frac{w_s}{P_s}(MR_{yy}-R_{sy}).
$$

The encoder accumulates these derivatives across hops and bands.
Covariance interpolation sends each hop's derivative to its two anchors
with weights $1-\alpha$ and $\alpha$.

### 7.4 Quantized search

The search combines continuous proposals with discrete acceptance:

1. Construct measured-power and previous-power candidates.
2. Test the selected quantizer scales.
3. Test the regular anchor grid and transient refinements.
4. Refine active powers with L-BFGS and an Armijo line search.
5. Quantize the retained optimization points and evaluate their complete RD cost.
6. Test activity suppression with the complete cost.
7. Rank correlation changes by their distortion differential.
8. Evaluate the promising quantized pair additions, replacements, and removals.
9. Refine powers again for the selected pair set.

Continuous refinement minimizes distortion. Quantization and the complete
RD evaluation decide whether to accept a proposal. This keeps the discrete
parameter rate in the decision without treating it as a smooth function.

The selected reconstructed state becomes the predictor for the next tile.
The search is local: it evaluates the current tile from a committed prior
state, rather than optimizing the whole programme jointly.

## 8. Encoder and decoder procedures

### Encoder

1. Apply source object gain and activity.
2. Fit and quantize the object trajectories.
3. Build the shared full-range downmix and the independent LFE signal.
4. Encode the core and obtain an aligned local decode.
5. Analyze source, ideal core, and decoded core with the shared transform.
6. Accumulate the moments and core-error envelopes.
7. Select quantized parameters with the tile RD search.
8. Fit, quantize, and rate-prune reconstruction-correction keyframes from the
   measured moments.
9. Predict and entropy-code the selected parameters and signal the selected
   correction keyframes.

### Decoder

1. Decode and align the audio core.
2. Reconstruct parameter predictors and object trajectories.
3. Analyze the seven full-range core signals.
4. Construct each anchor's positive-semidefinite covariance.
5. Interpolate covariance and error power at each hop.
6. Solve the regularized core system for each band.
7. Interpolate the reconstruction correction and add it to the solved matrix.
8. Apply the resulting matrix to each bin in that band.
9. Synthesize the object signals with normalized overlap-add.
10. Render the objects at the transmitted positions and carry LFE separately.

## 9. Computational structure

With $P$ selected pairs, forming $CD^H$ needs work proportional to
$(N+P)J$. Forming and solving the core system costs
$O((N+P)J^2+J^3)$ per band and hop. Applying the estimator costs $O(NJ)$
per spectral bin.

The core inverse stays small as $N$ grows. The object synthesis work and
power metadata grow with object count. Storing diagonal powers and pair
terms requires $O(N+P)$ covariance entries per band.

The reconstruction correction adds an $O(NJ^2)$ rank-$r$ factor fit per band
and keyframe; with $J=7$ this is small compared with the audio-rate STFT. The
decoder materializes $O(NJ)$ dense rows per band and keyframe and interpolates
them at $O(NJ)$ per hop. Its signaling cost scales with rank, sparse
support, and keyframe count, not with a dense $N\times N$ object covariance.

The algorithm relies on four shared quantities: reconstructed trajectories,
parameter predictor state, analysis timing, and the covariance and correction
interpolation rules. Agreement on these quantities makes the encoder's
candidate estimator the estimator that the decoder uses.

