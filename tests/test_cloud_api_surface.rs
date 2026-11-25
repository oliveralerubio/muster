//! Guards the public API that the private `muster-cloud` service imports.
//! If this stops compiling, the private repo breaks on its next rev bump.
//! Keep this list in step with every `muster::` path the service uses.

// Used by the private muster-cloud service.
#[allow(unused_imports)]
use muster::cloud_protocol::{self, SignedRequestV1};
#[allow(unused_imports)]
use muster::fleet::EnrollmentRequest;
#[allow(unused_imports)]
use muster::fleet_state::{
    seal, signer_sets, verify_against, FleetState, TrustAnchor, Verification, VerifyOptions,
};
#[allow(unused_imports)]
use muster::manifest::{Machine, Manifest, Member, MemberStatus, Role, TeamConfig};
#[allow(unused_imports)]
use muster::signing::{parse_public, verify, Signer};

#[test]
fn cloud_api_surface_is_public() {
    let _ = cloud_protocol::PROTOCOL_VERSION;
    let _ = (seal, signer_sets, verify_against, parse_public, verify);
    let _ = std::any::type_name::<(Signer, Manifest, EnrollmentRequest, SignedRequestV1)>();
    let _ = std::any::type_name::<(FleetState, TrustAnchor, Verification, VerifyOptions)>();
    let _ = std::any::type_name::<(Machine, Member, MemberStatus, Role, TeamConfig)>();
}
