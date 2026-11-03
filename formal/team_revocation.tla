------------------------- MODULE team_revocation -------------------------
\* Model of muster team mode: enrollment with admin quorum, revocation and
\* vault re-encryption. Mirrors src/team.rs, src/vault.rs,
\* src/commands/approve.rs and src/commands/revoke.rs.
EXTENDS Naturals, FiniteSets

CONSTANTS Machines, Admins, Quorum

ASSUME Quorum \in 1..Cardinality(Admins)

VARIABLES
    enrolled,       \* machines in the fleet manifest
    revoked,        \* machines whose access was revoked
    requests,       \* machines with a pending enrollment request
    approvals,      \* [machine -> set of admins that approved]
    activeAdmins,   \* admins that are active members
    vaultReaders    \* current vault recipient set (re-encrypted on change)

vars == <<enrolled, revoked, requests, approvals, activeAdmins, vaultReaders>>

RevokedMachines == revoked
CurrentVaultReaders == vaultReaders
\* Approvers that count: active admins only.
ActiveApprovers == UNION {approvals[m] : m \in Machines} \cap activeAdmins
\* Machines still acting as approvers (an enrolled machine owned by an admin).
ActiveApproverMachines == enrolled \ revoked

TypeOK ==
    /\ enrolled \subseteq Machines
    /\ revoked \subseteq Machines
    /\ requests \subseteq Machines
    /\ approvals \in [Machines -> SUBSET Admins]
    /\ activeAdmins \subseteq Admins
    /\ vaultReaders \subseteq Machines

Init ==
    /\ enrolled = {}
    /\ revoked = {}
    /\ requests = {}
    /\ approvals = [m \in Machines |-> {}]
    /\ activeAdmins = Admins
    /\ vaultReaders = {}

\* A machine asks to join. A revoked machine cannot re-request (its recipient
\* is forgotten only through a fresh identity, modelled as a new machine).
Enroll(m) ==
    /\ m \notin enrolled /\ m \notin requests /\ m \notin revoked
    /\ requests' = requests \cup {m}
    /\ UNCHANGED <<enrolled, revoked, approvals, activeAdmins, vaultReaders>>

\* An active admin records an approval; the same admin never counts twice.
Approve(m, a) ==
    /\ m \in requests /\ a \in activeAdmins /\ a \notin approvals[m]
    /\ approvals' = [approvals EXCEPT ![m] = @ \cup {a}]
    /\ UNCHANGED <<enrolled, revoked, requests, activeAdmins, vaultReaders>>

\* Quorum reached: machine joins and the vault is re-encrypted for it.
Admit(m) ==
    /\ m \in requests
    /\ Cardinality(approvals[m] \cap activeAdmins) >= Quorum
    /\ enrolled' = enrolled \cup {m}
    /\ requests' = requests \ {m}
    /\ vaultReaders' = vaultReaders \cup {m}
    /\ UNCHANGED <<revoked, approvals, activeAdmins>>

\* Revocation removes the machine from the fleet AND re-encrypts the vault
\* without it, atomically with respect to the invariant.
Revoke(m) ==
    /\ m \in enrolled
    /\ enrolled' = enrolled \ {m}
    /\ revoked' = revoked \cup {m}
    /\ vaultReaders' = vaultReaders \ {m}
    /\ UNCHANGED <<requests, approvals, activeAdmins>>

Next ==
    \/ \E m \in Machines : Enroll(m) \/ Admit(m) \/ Revoke(m)
    \/ \E m \in Machines, a \in Admins : Approve(m, a)

Spec == Init /\ [][Next]_vars

\* ---------------------------- Safety invariants ----------------------------
UnrevokedAccessOnly ==
    /\ \A m \in Machines : (m \in RevokedMachines => m \notin CurrentVaultReaders)
    /\ \A m \in Machines : (m \in RevokedMachines => m \notin ActiveApproverMachines)

\* Only active admins ever count toward a quorum.
ApprovalsFromActiveAdmins ==
    \A m \in Machines : Cardinality(approvals[m] \cap activeAdmins) <= Cardinality(activeAdmins)

=============================================================================
