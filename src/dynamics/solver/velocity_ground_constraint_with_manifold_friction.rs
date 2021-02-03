use super::{DeltaVel, SpringRegularization};
use crate::dynamics::solver::{AnyVelocityConstraint, VelocityGroundConstraint};
use crate::dynamics::{IntegrationParameters, RigidBodySet};
use crate::geometry::{ContactManifold, ContactManifoldIndex};
use crate::math::{AngVector, Point, Real, Vector, DIM, MAX_MANIFOLD_POINTS};
use crate::utils::{WAngularInertia, WBasis, WCross, WDot};
use simba::simd::SimdPartialOrd;

#[derive(Copy, Clone, Debug)]
struct VelocityConstraintElementPart {
    pub gcross2: AngVector<Real>,
    pub rhs: Real,
    pub impulse: Real,
    pub r: Real,
}

#[cfg(not(target_arch = "wasm32"))]
impl VelocityConstraintElementPart {
    fn zero() -> Self {
        Self {
            gcross2: na::zero(),
            rhs: 0.0,
            impulse: 0.0,
            r: 0.0,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct VelocityGroundConstraintWithManifoldFriction {
    dir1: Vector<Real>, // Non-penetration force direction for the first body.
    im2: Real,
    limit: Real,
    mj_lambda2: usize,
    manifold_id: ContactManifoldIndex,
    manifold_contact_id: usize,
    num_contacts: u8,
    normal_parts: [VelocityConstraintElementPart; MAX_MANIFOLD_POINTS],
    tangent_parts: [VelocityConstraintElementPart; DIM - 1],
    twist_part: VelocityConstraintElementPart,
    twist_weights: [Real; MAX_MANIFOLD_POINTS],
    impulse_scale: Real,
}

impl VelocityGroundConstraintWithManifoldFriction {
    #[cfg(feature = "parallel")]
    pub fn num_active_constraints(manifold: &ContactManifold) -> usize {
        let rest = manifold.data.solver_contacts.len() % MAX_MANIFOLD_POINTS != 0;
        manifold.data.solver_contacts.len() / MAX_MANIFOLD_POINTS + rest as usize
    }

    pub fn generate(
        params: &IntegrationParameters,
        manifold_id: ContactManifoldIndex,
        manifold: &ContactManifold,
        bodies: &RigidBodySet,
        out_constraints: &mut Vec<AnyVelocityConstraint>,
        push: bool,
    ) {
        let (erp, cfm, impulse_scale) =
            SpringRegularization::default().erp_cfm_impulse_scale(params.dt);

        let inv_dt = params.inv_dt();
        let mut rb1 = &bodies[manifold.data.body_pair.body1];
        let mut rb2 = &bodies[manifold.data.body_pair.body2];
        let flipped = !rb2.is_dynamic();

        let force_dir1 = if flipped {
            std::mem::swap(&mut rb1, &mut rb2);
            manifold.data.normal
        } else {
            -manifold.data.normal
        };

        let mj_lambda2 = rb2.island_offset;
        let warmstart_coeff = manifold.data.warmstart_multiplier * params.warmstart_coeff;

        for (l, manifold_points) in manifold
            .data
            .solver_contacts
            .chunks(MAX_MANIFOLD_POINTS)
            .enumerate()
        {
            #[cfg(not(target_arch = "wasm32"))]
            let mut constraint = VelocityGroundConstraintWithManifoldFriction {
                dir1: force_dir1,
                im2: rb2.effective_inv_mass,
                limit: 0.0,
                mj_lambda2,
                manifold_id,
                manifold_contact_id: l * MAX_MANIFOLD_POINTS,
                num_contacts: manifold_points.len() as u8,
                normal_parts: [VelocityConstraintElementPart::zero(); MAX_MANIFOLD_POINTS],
                tangent_parts: [VelocityConstraintElementPart::zero(); DIM - 1],
                twist_part: VelocityConstraintElementPart::zero(),
                twist_weights: [0.0; MAX_MANIFOLD_POINTS],
                impulse_scale,
            };

            let mut manifold_center = Point::origin();
            let mut tangent_impulses = [0.0, 0.0];

            for k in 0..manifold_points.len() {
                let manifold_point = &manifold_points[k];
                let dp1 = manifold_point.point - rb1.world_com;
                let dp2 = manifold_point.point - rb2.world_com;

                manifold_center += manifold_point.point.coords / (manifold_points.len() as Real);

                let vel1 = rb1.linvel + rb1.angvel.gcross(dp1);
                let vel2 = rb2.linvel + rb2.angvel.gcross(dp2);

                constraint.limit = manifold_point.friction;

                // Normal part.
                let gcross2 = rb2
                    .effective_world_inv_inertia_sqrt
                    .transform_vector(dp2.gcross(-force_dir1));

                let r = 1.0 / (cfm + rb2.effective_inv_mass + gcross2.gdot(gcross2));

                let mut rhs = (vel1 - vel2).dot(&force_dir1);

                if rhs <= -params.restitution_velocity_threshold {
                    rhs += manifold_point.restitution * rhs
                }

                if manifold_point.dist < 0.0 {
                    rhs += manifold_point.dist * erp;
                } else {
                    rhs += manifold_point.dist * inv_dt;
                }

                let impulse = manifold_point.data.impulse * warmstart_coeff;
                tangent_impulses[0] += manifold_point.data.tangent_impulse[0];
                tangent_impulses[1] += manifold_point.data.tangent_impulse[1];

                constraint.normal_parts[k] = VelocityConstraintElementPart {
                    gcross2,
                    rhs,
                    impulse,
                    r,
                };
            }

            // Tangent part.
            let tangents1 = force_dir1.orthonormal_basis();

            for j in 0..DIM - 1 {
                let dp1 = manifold_center - rb1.world_com;
                let dp2 = manifold_center - rb2.world_com;

                let vel1 = rb1.linvel + rb1.angvel.gcross(dp1);
                let vel2 = rb2.linvel + rb2.angvel.gcross(dp2);

                let gcross2 = rb2
                    .effective_world_inv_inertia_sqrt
                    .transform_vector(dp2.gcross(-tangents1[j]));
                let r = 1.0 / (rb2.effective_inv_mass + gcross2.gdot(gcross2));
                let rhs = (vel1 - vel2).dot(&tangents1[j]);
                let impulse = tangent_impulses[j] * warmstart_coeff;

                constraint.tangent_parts[j] = VelocityConstraintElementPart {
                    gcross2,
                    rhs,
                    impulse,
                    r,
                };
            }

            // Twist part.
            {
                for k in 0..manifold_points.len() {
                    constraint.twist_weights[k] =
                        na::distance(&manifold_points[k].point, &manifold_center);
                }

                let gcross2 = rb2
                    .effective_world_inv_inertia_sqrt
                    .transform_vector(-force_dir1);

                constraint.twist_part.r = crate::utils::inv(gcross2.norm_squared());
                constraint.twist_part.gcross2 = gcross2;
                constraint.twist_part.rhs = (rb1.angvel - rb2.angvel).gdot(force_dir1);
                constraint.twist_part.impulse = manifold.data.twist_impulse * warmstart_coeff;
            }

            #[cfg(not(target_arch = "wasm32"))]
            if push {
                out_constraints.push(AnyVelocityConstraint::NongroupedGround2(constraint));
            } else {
                out_constraints[manifold.data.constraint_index + l] =
                    AnyVelocityConstraint::NongroupedGround2(constraint);
            }
        }
    }

    pub fn warmstart(&self, mj_lambdas: &mut [DeltaVel<Real>]) {
        let mut mj_lambda2 = DeltaVel::zero();

        // Normal part.
        for i in 0..self.num_contacts as usize {
            let elt = &self.normal_parts[i];
            mj_lambda2.linear += self.dir1 * (-self.im2 * elt.impulse);
            mj_lambda2.angular += elt.gcross2 * elt.impulse;
        }

        // Tangent part.
        {
            // FIXME: move this out of the for loop?
            let tangents1 = self.dir1.orthonormal_basis();

            for j in 0..DIM - 1 {
                let elt = &self.tangent_parts[j];
                mj_lambda2.linear += tangents1[j] * (-self.im2 * elt.impulse);
                mj_lambda2.angular += elt.gcross2 * elt.impulse;
            }
        }

        // Twist part.
        {
            mj_lambda2.angular += self.twist_part.gcross2 * self.twist_part.impulse;
        }

        mj_lambdas[self.mj_lambda2 as usize].linear += mj_lambda2.linear;
        mj_lambdas[self.mj_lambda2 as usize].angular += mj_lambda2.angular;
    }

    pub fn solve(&mut self, mj_lambdas: &mut [DeltaVel<Real>]) {
        let mut mj_lambda2 = mj_lambdas[self.mj_lambda2 as usize];

        // Solve friction.
        let tangents1 = self.dir1.orthonormal_basis();
        let friction_limit = self.limit
            * (self.normal_parts[0].impulse
                + self.normal_parts[1].impulse
                + self.normal_parts[2].impulse
                + self.normal_parts[3].impulse);

        for j in 0..DIM - 1 {
            let elt = &mut self.tangent_parts[j];
            let dimpulse = -tangents1[j].dot(&mj_lambda2.linear)
                + elt.gcross2.gdot(mj_lambda2.angular)
                + elt.rhs;
            let new_impulse =
                (elt.impulse - elt.r * dimpulse).simd_clamp(-friction_limit, friction_limit);
            let dlambda = new_impulse - elt.impulse;
            elt.impulse = new_impulse;

            mj_lambda2.linear += tangents1[j] * (-self.im2 * dlambda);
            mj_lambda2.angular += elt.gcross2 * dlambda;
        }

        // Solve non-penetration.
        for i in 0..self.num_contacts as usize {
            let elt = &mut self.normal_parts[i];
            let dimpulse =
                -self.dir1.dot(&mj_lambda2.linear) + elt.gcross2.gdot(mj_lambda2.angular) + elt.rhs;
            let new_impulse = (elt.impulse * self.impulse_scale - elt.r * dimpulse).max(0.0);
            let dlambda = new_impulse - elt.impulse;
            elt.impulse = new_impulse;

            mj_lambda2.linear += self.dir1 * (-self.im2 * dlambda);
            mj_lambda2.angular += elt.gcross2 * dlambda;
        }

        // Solve twist.
        {
            let twist_limit = self.limit
                * (self.normal_parts[0].impulse * self.twist_weights[0]
                    + self.normal_parts[1].impulse * self.twist_weights[1]
                    + self.normal_parts[2].impulse * self.twist_weights[2]
                    + self.normal_parts[3].impulse * self.twist_weights[3]);

            let dimpulse = self.twist_part.gcross2.gdot(mj_lambda2.angular) + self.twist_part.rhs;
            let new_impulse = (self.twist_part.impulse - self.twist_part.r * dimpulse)
                .simd_clamp(-twist_limit, twist_limit);
            let dlambda = new_impulse - self.twist_part.impulse;
            self.twist_part.impulse = new_impulse;

            mj_lambda2.angular += self.twist_part.gcross2 * dlambda;
        }

        mj_lambdas[self.mj_lambda2 as usize] = mj_lambda2;
    }

    pub fn writeback_impulses(&self, manifolds_all: &mut [&mut ContactManifold]) {
        let manifold = &mut manifolds_all[self.manifold_id];
        let k_base = self.manifold_contact_id;
        let denom = crate::utils::inv(
            self.normal_parts[0].impulse
                + self.normal_parts[1].impulse
                + self.normal_parts[2].impulse
                + self.normal_parts[3].impulse,
        );

        for k in 0..self.num_contacts as usize {
            let active_contacts = &mut manifold.points[..manifold.data.num_active_contacts()];
            let normal_factor = self.normal_parts[k].impulse * denom;
            active_contacts[k_base + k].data.impulse = self.normal_parts[k].impulse;
            active_contacts[k_base + k].data.tangent_impulse = [
                self.tangent_parts[0].impulse * normal_factor,
                self.tangent_parts[1].impulse * normal_factor,
            ];
        }

        manifold.data.twist_impulse = self.twist_part.impulse;
    }
}