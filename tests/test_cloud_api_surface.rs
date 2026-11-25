//! Guards the public API that the private `muster-cloud` service imports.
//! If this stops compiling, the private repo breaks on its next rev bump.

// Used by the private muster-cloud service.
#[allow(unused_imports)]
use muster::cloud_protocol;
#[allow(unused_imports)]
use muster::fleet::EnrollmentRequest;
#[allow(unused_imports)]
use muster::fleet_state::seal;
#[allow(unused_imports)]
use muster::manifest::Manifest;
#[allow(unused_imports)]
use muster::signing::Signer;

#[test]
fn cloud_api_surface_is_public() {
    let _ = cloud_protocol::PROTOCOL_VERSION;
    let _ = seal;
    let _ = std::any::type_name::<(Signer, Manifest, EnrollmentRequest)>();
}
