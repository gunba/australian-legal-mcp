targetScope = 'subscription'

@description('Name of the dedicated disposable test resource group.')
param resourceGroupName string = 'rg-australian-legal-mcp-test'

@description('Azure region for compute and storage. Australia East is the supported default.')
param location string = 'australiaeast'

@description('Short lowercase prefix used for resource names and the public DNS label.')
@minLength(3)
@maxLength(18)
param namePrefix string = 'legalmcptest'

@description('Linux break-glass administrator account name.')
param adminUsername string = 'azureadmin'

@secure()
@description('OpenSSH public key for the administrator account.')
param adminSshPublicKey string

@secure()
@description('A distinct OpenSSH public key restricted to the generation publisher forced command.')
param publisherSshPublicKey string

@description('The one trusted public IPv4 address allowed to SSH, for example 203.0.113.4.')
param adminSourceIp string

@description('Expose ports 80 and 443 for Caddy and Microsoft cloud connectors. Keep false until Entra auth is configured.')
param publicMcpEnabled bool = false

@allowed(['Standard_B2s_v2'])
@description('Pinned x86-64 2-vCPU, 8-GiB test VM. Confirm availability in the subscription before deployment.')
param vmSize string = 'Standard_B2s_v2'

@allowed([128, 256])
@description('Persistent Standard SSD data disk. 128 GiB relies on CoW delta deployment; choose 256 GiB for full-copy headroom.')
param dataDiskSizeGiB int = 128

@description('Optional object ID granted Storage Blob Data Contributor for local generation upload. Leave empty and assign later if preferred.')
param uploaderPrincipalId string = ''

@description('Daily Azure VM auto-shutdown time in HHmm, interpreted in autoShutdownTimeZone.')
param autoShutdownTime string = '1900'

@description('Windows time-zone ID used by Azure auto-shutdown.')
param autoShutdownTimeZone string = 'W. Australia Standard Time'

@description('Optional monthly budget in the subscription billing currency. Set to 0 to omit the budget resource.')
@minValue(0)
param monthlyBudgetAmount int = 0

@description('Email for budget actual/forecast notifications. Required when monthlyBudgetAmount is nonzero.')
param budgetContactEmail string = ''

@description('Budget start date. utcNow is evaluated only when a value is not supplied at deployment time.')
param budgetStartDate string = utcNow('yyyy-MM-01')

resource resourceGroup 'Microsoft.Resources/resourceGroups@2024-03-01' = {
  name: resourceGroupName
  location: location
  tags: {
    application: 'australian-legal-mcp'
    environment: 'test'
    workload: 'mcp'
    dataClassification: 'public-legal-source-content'
    expiresOn: 'review-manually'
  }
}

module stack './stack.bicep' = {
  name: 'australian-legal-mcp-test-stack'
  scope: resourceGroup
  params: {
    location: location
    namePrefix: namePrefix
    adminUsername: adminUsername
    adminSshPublicKey: adminSshPublicKey
    publisherSshPublicKey: publisherSshPublicKey
    adminSourceIp: adminSourceIp
    publicMcpEnabled: publicMcpEnabled
    vmSize: vmSize
    dataDiskSizeGiB: dataDiskSizeGiB
    uploaderPrincipalId: uploaderPrincipalId
    autoShutdownTime: autoShutdownTime
    autoShutdownTimeZone: autoShutdownTimeZone
  }
}

module budget './budget.bicep' = if (monthlyBudgetAmount > 0) {
  name: 'australian-legal-mcp-test-budget'
  params: {
    budgetName: '${namePrefix}-monthly'
    resourceGroupName: resourceGroupName
    amount: monthlyBudgetAmount
    contactEmail: budgetContactEmail
    startDate: budgetStartDate
  }
  dependsOn: [resourceGroup]
}

output resourceGroupName string = resourceGroup.name
output vmName string = stack.outputs.vmName
output publicIpAddress string = stack.outputs.publicIpAddress
output publicHost string = stack.outputs.publicHost
output mcpUrl string = 'https://${stack.outputs.publicHost}/mcp'
output blobBaseUrl string = stack.outputs.blobBaseUrl
output storageAccountName string = stack.outputs.storageAccountName
output dataDiskName string = stack.outputs.dataDiskName
output managedIdentityPrincipalId string = stack.outputs.managedIdentityPrincipalId
output publisherUsername string = stack.outputs.publisherUsername
