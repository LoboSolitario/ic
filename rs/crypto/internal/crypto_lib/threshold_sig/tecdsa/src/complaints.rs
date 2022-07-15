use crate::*;
use ic_types::crypto::canister_threshold_sig::idkg::IDkgComplaint;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::convert::TryFrom;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IDkgComplaintInternal {
    pub(crate) proof: zk::ProofOfDLogEquivalence,
    pub(crate) shared_secret: EccPoint,
}

impl IDkgComplaintInternal {
    pub fn serialize(&self) -> ThresholdEcdsaResult<Vec<u8>> {
        serde_cbor::to_vec(self)
            .map_err(|e| ThresholdEcdsaError::SerializationError(format!("{}", e)))
    }

    pub fn deserialize(bytes: &[u8]) -> ThresholdEcdsaResult<Self> {
        serde_cbor::from_slice::<Self>(bytes)
            .map_err(|e| ThresholdEcdsaError::SerializationError(format!("{}", e)))
    }
}

pub fn generate_complaints(
    verified_dealings: &BTreeMap<NodeIndex, IDkgDealingInternal>,
    associated_data: &[u8],
    receiver_index: NodeIndex,
    secret_key: &MEGaPrivateKey,
    public_key: &MEGaPublicKey,
    seed: Seed,
) -> ThresholdEcdsaResult<BTreeMap<NodeIndex, IDkgComplaintInternal>> {
    let mut complaints = BTreeMap::new();

    for (dealer_index, dealing) in verified_dealings {
        // Decrypt each dealing and check consistency with the commitment in the dealing
        let opening = dealing.ciphertext.decrypt_and_check(
            &dealing.commitment,
            associated_data,
            *dealer_index,
            receiver_index,
            secret_key,
            public_key,
        );

        if opening.is_err() {
            let complaint_seed = seed.derive(&format!(
                "ic-crypto-tecdsa-complaint-against-{}",
                dealer_index
            ));

            let complaint = IDkgComplaintInternal::new(
                complaint_seed,
                dealing,
                *dealer_index,
                receiver_index,
                secret_key,
                public_key,
                associated_data,
            )?;

            complaints.insert(*dealer_index, complaint);
        }
    }

    if complaints.is_empty() {
        return Err(ThresholdEcdsaError::InvalidArguments(
            "generate_complaints should return at least one complaint".to_string(),
        ));
    }

    Ok(complaints)
}

impl IDkgComplaintInternal {
    /// Create a new complaint against a dealing
    ///
    /// This should only be done in a situation where the commitment opening
    /// contained within the MEGa ciphertext of the dealing is decrypted,
    /// and does not match the dealing commitment.
    pub fn new(
        seed: Seed,
        dealing: &IDkgDealingInternal,
        dealer_index: NodeIndex,
        receiver_index: NodeIndex,
        secret_key: &MEGaPrivateKey,
        public_key: &MEGaPublicKey,
        associated_data: &[u8],
    ) -> ThresholdEcdsaResult<Self> {
        let shared_secret = dealing
            .ciphertext
            .ephemeral_key()
            .scalar_mul(secret_key.secret_scalar())?;

        let proof_assoc_data = Self::create_proof_assoc_data(
            associated_data,
            receiver_index,
            dealer_index,
            public_key,
        )?;

        let proof = zk::ProofOfDLogEquivalence::create(
            seed,
            secret_key.secret_scalar(),
            &EccPoint::generator_g(secret_key.secret_scalar().curve_type())?,
            dealing.ciphertext.ephemeral_key(),
            &proof_assoc_data,
        )?;

        Ok(Self {
            shared_secret,
            proof,
        })
    }

    /// Verify a complaint
    ///
    /// This checks that a particular complaint, generated by a complainer
    /// with the specified index and public key, is in fact a valid complaint.
    ///
    /// Specifically, using information provided by the complainer, it decrypts
    /// the MEGa ciphertext and checks that the plaintext is not a valid opening
    /// to the dealing commitment. A ZK proof prevents the complainer from
    /// making a false complaint.
    pub fn verify(
        &self,
        dealing: &IDkgDealingInternal,
        dealer_index: NodeIndex,
        complainer_index: NodeIndex,
        complainer_key: &MEGaPublicKey,
        associated_data: &[u8],
    ) -> ThresholdEcdsaResult<()> {
        // Verify the enclosed proof
        let proof_assoc_data = Self::create_proof_assoc_data(
            associated_data,
            complainer_index,
            dealer_index,
            complainer_key,
        )?;

        self.proof.verify(
            &EccPoint::generator_g(self.shared_secret.curve_type())?,
            dealing.ciphertext.ephemeral_key(),
            complainer_key.public_point(),
            &self.shared_secret,
            &proof_assoc_data,
        )?;

        // Decrypt the ciphertext using the proven shared secret
        let opening = match (&dealing.ciphertext, &dealing.commitment) {
            (&MEGaCiphertext::Single(ref c), &PolynomialCommitment::Simple(_)) => {
                let opening = c.decrypt_from_shared_secret(
                    associated_data,
                    dealer_index,
                    complainer_index,
                    complainer_key,
                    &self.shared_secret,
                )?;

                CommitmentOpening::Simple(opening)
            }
            (&MEGaCiphertext::Pairs(ref c), &PolynomialCommitment::Pedersen(_)) => {
                let opening = c.decrypt_from_shared_secret(
                    associated_data,
                    dealer_index,
                    complainer_index,
                    complainer_key,
                    &self.shared_secret,
                )?;

                CommitmentOpening::Pedersen(opening.0, opening.1)
            }
            (_, _) => return Err(ThresholdEcdsaError::UnexpectedCommitmentType),
        };

        // Verify that the decrypted opening does *not* match the
        // dealing commitment

        if dealing
            .commitment
            .check_opening(complainer_index, &opening)?
        {
            return Err(ThresholdEcdsaError::InvalidComplaint);
        }

        Ok(())
    }

    fn create_proof_assoc_data(
        associated_data: &[u8],
        receiver_index: NodeIndex,
        dealer_index: NodeIndex,
        public_key: &MEGaPublicKey,
    ) -> ThresholdEcdsaResult<Vec<u8>> {
        let mut ro = ro::RandomOracle::new("ic-crypto-tecdsa-complaint-proof-assoc-data");

        ro.add_bytestring("associated_data", associated_data)?;
        ro.add_u32("receiver_index", receiver_index)?;
        ro.add_u32("dealer_index", dealer_index)?;
        ro.add_point("receiver_public_key", public_key.public_point())?;

        ro.output_bytestring(32)
    }
}

impl TryFrom<&IDkgComplaint> for IDkgComplaintInternal {
    type Error = ThresholdEcdsaError;

    fn try_from(complaint: &IDkgComplaint) -> ThresholdEcdsaResult<Self> {
        Self::deserialize(&complaint.internal_complaint_raw)
    }
}
