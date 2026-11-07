//! The field-name tables upstream balance payloads are read through.

/// WorkBuddy/CodeBuddy billing host. The CLI chat endpoint (`copilot.tencent.com`)
/// serves inference; billing lives on the product site.
pub const WORKBUDDY_BILLING_HOST: &str = "https://www.codebuddy.cn";

pub const WORKBUDDY_RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";

/// Product code used by the official balance console for CodeBuddy packages.
pub const WORKBUDDY_RESOURCE_PRODUCT_CODE: &str = "p_tcaca";

/// Fields that may carry the *remaining* credit amount of a package.
pub(crate) const REMAINING_FIELDS: &[&str] = &[
    "SlicePeriodCapacityRemainPrecise",
    "SlicePeriodCapacityRemain",
    "CycleCapacityRemainPrecise",
    "CycleCapacityRemain",
    "CapacityRemainPrecise",
    "CapacityRemain",
    "RemainPrecise",
    "Remain",
    "Remaining",
    "Balance",
];

/// Fields that may carry the *total* capacity of a package.
pub(crate) const TOTAL_FIELDS: &[&str] = &[
    "SlicePeriodCapacitySizePrecise",
    "SlicePeriodCapacitySize",
    "CycleCapacitySizePrecise",
    "CycleCapacitySize",
    "CycleCapacityPrecise",
    "CycleCapacity",
    "CapacityPrecise",
    "Capacity",
    "TotalCapacityPrecise",
    "TotalCapacity",
    "PackageCapacity",
    "Quota",
    "Amount",
];

/// Fields that may carry a package expiry timestamp.
pub(crate) const EXPIRY_FIELDS: &[&str] = &[
    "DeductionEndTime",
    "ExpiredTime",
    "SlicePeriodEndTime",
    "PackageEndTime",
    "EndTime",
    "CycleEndTime",
    "ExpireTime",
    "ExpirationTime",
    "ValidEndTime",
    "ValidPeriodEndTime",
    "EndAt",
    "ExpireAt",
];

/// Fields that may carry a human-readable package label.
pub(crate) const LABEL_FIELDS: &[&str] = &[
    "PackageName",
    "PackageTypeName",
    "AccountName",
    "ProductName",
    "Name",
    "RuleName",
    "Description",
];

/// Account-level summary remaining fields, used when aggregating the total
/// wallet balance (mirrors the upstream `CycleCapacityRemain*` priority).
pub(crate) const SUMMARY_REMAINING_FIELDS: &[&str] = &[
    "CycleCapacityRemainPrecise",
    "CycleCapacityRemain",
    "CapacityRemainPrecise",
    "CapacityRemain",
];
