#!/usr/bin/swift
import CryptoKit
import Darwin
import Foundation
import Security

private let bundleIdentifier = "com.manuelgozzi.monhop"
private let certificateCommonName = "MonHop Development Signing"
private let keychainFilename = "login.keychain-db"
private let codesignPath = "/usr/bin/codesign"
private let opensslPath = "/usr/bin/openssl"
private let setupLockFilename = "macos-signing.setup.lock"
// These legacy Security enums are present in the SDK but not named in Swift.
private let pkcs12Format = SecExternalFormat(rawValue: 12)!
private let aggregateItemType = SecExternalItemType(rawValue: 5)!

private enum SigningError: Error, CustomStringConvertible {
    case message(String)

    var description: String {
        switch self {
        case let .message(value): value
        }
    }
}

private struct SigningIdentity {
    let certificate: SecCertificate
    let fingerprint: String
    let loginKeychainPath: String
}

private final class SetupLock {
    private let descriptor: Int32

    init(descriptor: Int32) {
        self.descriptor = descriptor
    }

    deinit {
        close(descriptor)
    }
}

private func require(_ condition: Bool, _ message: String) throws {
    guard condition else {
        throw SigningError.message(message)
    }
}

private func requireSuccess(_ status: OSStatus, _ operation: String) throws {
    guard status == errSecSuccess else {
        throw SigningError.message("\(operation) failed (OSStatus \(status)).")
    }
}

private func loginKeychain() throws -> (reference: SecKeychain, path: String) {
    let path = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Keychains", isDirectory: true)
        .appendingPathComponent(keychainFilename)
        .path
    try require(FileManager.default.fileExists(atPath: path), "The current user's login keychain is unavailable at \(path).")

    var keychain: SecKeychain?
    try requireSuccess(SecKeychainOpen(path, &keychain), "Opening the login keychain")
    guard let keychain else {
        throw SigningError.message("Opening the login keychain returned no keychain.")
    }
    return (keychain, path)
}

private func allCertificates(in keychain: SecKeychain) throws -> [SecCertificate] {
    let query: [CFString: Any] = [
        kSecClass: kSecClassCertificate,
        kSecMatchSearchList: [keychain],
        kSecMatchLimit: kSecMatchLimitAll,
        kSecReturnRef: true,
    ]
    var result: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &result)
    if status == errSecItemNotFound {
        return []
    }
    try requireSuccess(status, "Inspecting login-keychain certificates")
    guard let certificates = result as? [SecCertificate] else {
        throw SigningError.message("Inspecting login-keychain certificates returned an unexpected result.")
    }
    return certificates
}

private func allIdentities(in keychain: SecKeychain) throws -> [SecIdentity] {
    let query: [CFString: Any] = [
        kSecClass: kSecClassIdentity,
        kSecMatchSearchList: [keychain],
        kSecMatchLimit: kSecMatchLimitAll,
        kSecReturnRef: true,
    ]
    var result: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &result)
    if status == errSecItemNotFound {
        return []
    }
    try requireSuccess(status, "Inspecting login-keychain identities")
    guard let identities = result as? [SecIdentity] else {
        throw SigningError.message("Inspecting login-keychain identities returned an unexpected result.")
    }
    return identities
}

private func monHopSigningPrivateKeys(in keychain: SecKeychain) throws -> [[CFString: Any]] {
    let query: [CFString: Any] = [
        kSecClass: kSecClassKey,
        kSecAttrKeyClass: kSecAttrKeyClassPrivate,
        kSecMatchSearchList: [keychain],
        kSecMatchLimit: kSecMatchLimitAll,
        kSecReturnAttributes: true,
    ]
    var result: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &result)
    if status == errSecItemNotFound {
        return []
    }
    try requireSuccess(status, "Inspecting login-keychain private keys")
    guard let attributes = result as? [[CFString: Any]] else {
        throw SigningError.message("Inspecting login-keychain private keys returned an unexpected result.")
    }
    return attributes.filter { $0[kSecAttrLabel] as? String == certificateCommonName }
}

private func setupLock() throws -> SetupLock {
    let directory = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Caches/MonHop", isDirectory: true)
    if !FileManager.default.fileExists(atPath: directory.path) {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    }
    var directoryStatus = stat()
    try require(lstat(directory.path, &directoryStatus) == 0, "Inspecting the MonHop setup-lock directory failed: \(String(cString: strerror(errno))).")
    try require((directoryStatus.st_mode & mode_t(S_IFMT)) == mode_t(S_IFDIR), "The MonHop setup-lock directory is not a directory.")
    try require(directoryStatus.st_uid == getuid(), "The MonHop setup-lock directory is not owned by the current user.")
    try require((directoryStatus.st_mode & 0o077) == 0, "The MonHop setup-lock directory is accessible by another user.")

    let lockPath = directory.appendingPathComponent(setupLockFilename).path
    let descriptor = open(lockPath, O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC, mode_t(0o600))
    try require(descriptor >= 0, "Opening the MonHop setup lock failed: \(String(cString: strerror(errno))).")
    var lockStatus = stat()
    guard fstat(descriptor, &lockStatus) == 0 else {
        close(descriptor)
        throw SigningError.message("Inspecting the MonHop setup lock failed: \(String(cString: strerror(errno))).")
    }
    guard (lockStatus.st_mode & mode_t(S_IFMT)) == mode_t(S_IFREG), lockStatus.st_uid == getuid(), (lockStatus.st_mode & 0o077) == 0 else {
        close(descriptor)
        throw SigningError.message("The MonHop setup lock has unsafe ownership, type, or permissions.")
    }
    guard flock(descriptor, LOCK_EX) == 0 else {
        close(descriptor)
        throw SigningError.message("Locking MonHop signing setup failed: \(String(cString: strerror(errno))).")
    }
    return SetupLock(descriptor: descriptor)
}

private func commonName(of certificate: SecCertificate) -> String? {
    var commonName: CFString?
    guard SecCertificateCopyCommonName(certificate, &commonName) == errSecSuccess else {
        return nil
    }
    return commonName as String?
}

private func fingerprint(of certificate: SecCertificate) -> String {
    let data = SecCertificateCopyData(certificate) as Data
    return Insecure.SHA1.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

private func identityCertificate(_ identity: SecIdentity) throws -> SecCertificate {
    var certificate: SecCertificate?
    try requireSuccess(SecIdentityCopyCertificate(identity, &certificate), "Reading signing identity certificate")
    guard let certificate else {
        throw SigningError.message("The signing identity has no certificate.")
    }
    return certificate
}

private func certificateProperties(_ certificate: SecCertificate) throws -> [CFString: Any] {
    let requested = [kSecOIDX509V1ValidityNotBefore, kSecOIDX509V1ValidityNotAfter, kSecOIDExtendedKeyUsage] as CFArray
    var error: Unmanaged<CFError>?
    guard let values = SecCertificateCopyValues(certificate, requested, &error) as? [CFString: Any] else {
        let detail = error?.takeRetainedValue().localizedDescription ?? "no certificate values returned"
        throw SigningError.message("Reading MonHop signing certificate properties failed: \(detail).")
    }
    return values
}

private func validateCertificate(_ certificate: SecCertificate) throws {
    let subject = SecCertificateCopyNormalizedSubjectSequence(certificate) as Data?
    let issuer = SecCertificateCopyNormalizedIssuerSequence(certificate) as Data?
    try require(subject != nil && subject == issuer, "The MonHop signing certificate is not self-signed.")
    try verifySelfSignature(certificate)

    let values = try certificateProperties(certificate)
    guard let notBeforeValue = (values[kSecOIDX509V1ValidityNotBefore] as? [CFString: Any])?[kSecPropertyKeyValue] as? NSNumber,
          let notAfterValue = (values[kSecOIDX509V1ValidityNotAfter] as? [CFString: Any])?[kSecPropertyKeyValue] as? NSNumber else {
        throw SigningError.message("The MonHop signing certificate has no readable validity dates.")
    }
    let notBefore = Date(timeIntervalSinceReferenceDate: notBeforeValue.doubleValue)
    let notAfter = Date(timeIntervalSinceReferenceDate: notAfterValue.doubleValue)
    try require(notBefore <= Date(), "The MonHop signing certificate is not yet valid.")
    try require(notAfter > Date(), "The MonHop signing certificate is expired.")
    guard let extendedKeyUsage = (values[kSecOIDExtendedKeyUsage] as? [CFString: Any])?[kSecPropertyKeyValue] as? [Data] else {
        throw SigningError.message("The MonHop signing certificate has no readable code-signing EKU.")
    }
    let codeSigningOID = Data([0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03])
    try require(extendedKeyUsage.contains(codeSigningOID), "The MonHop signing certificate does not permit code signing.")
}

private func verifySelfSignature(_ certificate: SecCertificate) throws {
    let certificateData = SecCertificateCopyData(certificate) as Data
    let parts = try certificateDERParts(certificateData)
    guard let publicKey = SecCertificateCopyKey(certificate) else {
        throw SigningError.message("The MonHop signing certificate has no public key.")
    }
    var error: Unmanaged<CFError>?
    try require(
        SecKeyVerifySignature(publicKey, .rsaSignatureMessagePKCS1v15SHA256, parts.tbs as CFData, parts.signature as CFData, &error),
        "The MonHop signing certificate does not validate its own signature: \(error?.takeRetainedValue().localizedDescription ?? "unknown error")."
    )
    try require(!(try certificateIsAuthority(parts.tbs)), "The MonHop signing certificate is a certificate authority.")
}

private func certificateDERParts(_ certificate: Data) throws -> (tbs: Data, signature: Data) {
    let outer = try derElement(in: certificate, at: 0)
    try require(outer.tag == 0x30 && outer.end == certificate.count, "The MonHop signing certificate is not a DER sequence.")
    let tbs = try derElement(in: certificate, at: outer.contentStart)
    let algorithm = try derElement(in: certificate, at: tbs.end)
    let signature = try derElement(in: certificate, at: algorithm.end)
    try require(tbs.tag == 0x30 && algorithm.tag == 0x30 && signature.tag == 0x03 && signature.end == outer.end, "The MonHop signing certificate has an invalid DER layout.")
    let sha256WithRSA = Data([0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B])
    try require(algorithm.content.contains(sha256WithRSA), "The MonHop signing certificate does not use SHA-256 with RSA.")
    try require(signature.content.count > 1 && signature.content[signature.content.startIndex] == 0, "The MonHop signing certificate has an invalid signature bit string.")
    return (tbs.full, Data(signature.content.dropFirst()))
}

private func certificateIsAuthority(_ tbsCertificate: Data) throws -> Bool {
    let tbsFields = try derChildren(in: tbsCertificate)
    let extensionContainers = tbsFields.filter { $0.tag == 0xA3 }
    try require(extensionContainers.count == 1, "The MonHop signing certificate has no readable basic-constraints extension.")
    let extensionSequence = try derElement(in: extensionContainers[0].content, at: 0)
    try require(extensionSequence.tag == 0x30 && extensionSequence.end == extensionContainers[0].content.count, "The MonHop signing certificate has an invalid extensions sequence.")

    let basicConstraintsOID = Data([0x55, 0x1D, 0x13])
    let matchingExtensions = try derChildren(in: extensionSequence.full).filter { candidate in
        let fields = try derChildren(in: candidate.full)
        return fields.first?.tag == 0x06 && fields.first?.content == basicConstraintsOID
    }
    try require(matchingExtensions.count == 1, "The MonHop signing certificate has no unique basic-constraints extension.")

    let extensionFields = try derChildren(in: matchingExtensions[0].full)
    guard let encodedConstraints = extensionFields.last, encodedConstraints.tag == 0x04 else {
        throw SigningError.message("The MonHop signing certificate has invalid basic constraints.")
    }
    let constraints = try derElement(in: encodedConstraints.content, at: 0)
    try require(constraints.tag == 0x30 && constraints.end == encodedConstraints.content.count, "The MonHop signing certificate has invalid basic constraints.")
    let fields = try derChildren(in: constraints.full)
    if fields.isEmpty {
        return false
    }
    guard let certificateAuthority = fields.first, certificateAuthority.tag == 0x01 else {
        throw SigningError.message("The MonHop signing certificate has invalid basic constraints.")
    }
    try require(fields.count <= 2 && (fields.count == 1 || fields[1].tag == 0x02), "The MonHop signing certificate has invalid basic constraints.")
    try require(certificateAuthority.content.count == 1, "The MonHop signing certificate has invalid basic constraints.")
    try require(certificateAuthority.content[certificateAuthority.content.startIndex] != 0 || fields.count == 1, "The MonHop signing certificate has invalid basic constraints.")
    return certificateAuthority.content[certificateAuthority.content.startIndex] != 0
}

private func derChildren(in sequence: Data) throws -> [(tag: UInt8, contentStart: Int, content: Data, full: Data, end: Int)] {
    let container = try derElement(in: sequence, at: 0)
    try require(container.tag == 0x30 && container.end == sequence.count, "The MonHop signing certificate contains an invalid DER sequence.")
    var elements: [(tag: UInt8, contentStart: Int, content: Data, full: Data, end: Int)] = []
    var offset = container.contentStart
    while offset < container.end {
        let element = try derElement(in: sequence, at: offset)
        elements.append(element)
        offset = element.end
    }
    try require(offset == container.end, "The MonHop signing certificate contains truncated DER content.")
    return elements
}

private func derElement(in data: Data, at offset: Int) throws -> (tag: UInt8, contentStart: Int, content: Data, full: Data, end: Int) {
    let bytes = [UInt8](data)
    try require(offset + 2 <= bytes.count, "The MonHop signing certificate contains truncated DER.")
    let tag = bytes[offset]
    let lengthMarker = bytes[offset + 1]
    var contentLength = 0
    var contentStart = offset + 2
    if lengthMarker & 0x80 == 0 {
        contentLength = Int(lengthMarker)
    } else {
        let lengthBytes = Int(lengthMarker & 0x7F)
        try require(lengthBytes > 0 && lengthBytes <= 4 && contentStart + lengthBytes <= bytes.count, "The MonHop signing certificate has an invalid DER length.")
        for index in 0..<lengthBytes {
            contentLength = (contentLength << 8) | Int(bytes[contentStart + index])
        }
        contentStart += lengthBytes
    }
    let end = contentStart + contentLength
    try require(end <= bytes.count, "The MonHop signing certificate contains truncated DER content.")
    return (tag, contentStart, Data(bytes[contentStart..<end]), Data(bytes[offset..<end]), end)
}

private func trustedApplicationData(_ application: SecTrustedApplication) throws -> Data {
    var data: CFData?
    try requireSuccess(SecTrustedApplicationCopyData(application, &data), "Reading a signing-key trusted application")
    guard let data else {
        throw SigningError.message("Reading a signing-key trusted application returned no data.")
    }
    return data as Data
}

private func validateCodesignAccess(_ access: SecAccess) throws {
    var expectedApplication: SecTrustedApplication?
    try requireSuccess(SecTrustedApplicationCreateFromPath(codesignPath, &expectedApplication), "Reading the expected codesign access rule")
    guard let expectedApplication else {
        throw SigningError.message("Reading the expected codesign access rule returned no application.")
    }
    let expectedData = try trustedApplicationData(expectedApplication)

    var aclList: CFArray?
    try requireSuccess(SecAccessCopyACLList(access, &aclList), "Reading MonHop signing-key ACL entries")
    guard let acls = aclList as? [SecACL] else {
        throw SigningError.message("The MonHop signing key has no readable ACL entries.")
    }

    var allowsCodesignToSign = false
    for acl in acls {
        let authorizations = SecACLCopyAuthorizations(acl) as? [CFString] ?? []
        let permitsSigning = authorizations.contains(kSecACLAuthorizationSign) || authorizations.contains(kSecACLAuthorizationAny)
        guard permitsSigning else {
            continue
        }
        var applications: CFArray?
        var description: CFString?
        var prompt = SecKeychainPromptSelector()
        try requireSuccess(SecACLCopyContents(acl, &applications, &description, &prompt), "Reading a MonHop signing-key ACL entry")
        guard let applications = applications as? [SecTrustedApplication], applications.count == 1 else {
            throw SigningError.message("The MonHop signing key grants signing access to more than codesign.")
        }
        try require(try trustedApplicationData(applications[0]) == expectedData, "The MonHop signing key grants signing access to an application other than codesign.")
        allowsCodesignToSign = true
    }
    try require(allowsCodesignToSign, "The MonHop signing key does not grant signing access to codesign.")
}

private func validateCodesignAccess(for privateKey: SecKey) throws {
    // Imported login-keychain private keys are also legacy SecKeychainItem values.
    let keychainItem = unsafeBitCast(privateKey, to: SecKeychainItem.self)
    var access: SecAccess?
    try requireSuccess(SecKeychainItemCopyAccess(keychainItem, &access), "Reading the MonHop signing-key access rule")
    guard let access else {
        throw SigningError.message("The MonHop signing key has no access rule.")
    }
    try validateCodesignAccess(access)
}

private func validateIdentity(_ identity: SecIdentity, certificate: SecCertificate, loginKeychainPath: String) throws -> SigningIdentity {
    let subject = SecCertificateCopyNormalizedSubjectSequence(certificate) as Data?
    let issuer = SecCertificateCopyNormalizedIssuerSequence(certificate) as Data?
    try require(subject != nil && subject == issuer, "The MonHop signing certificate is not self-signed.")

    var privateKey: SecKey?
    try requireSuccess(SecIdentityCopyPrivateKey(identity, &privateKey), "Reading signing identity private key")
    guard let privateKey else {
        throw SigningError.message("The MonHop signing certificate has no private key.")
    }
    let attributes = SecKeyCopyAttributes(privateKey) as? [CFString: Any]
    try require(attributes?[kSecAttrKeyType] as? String == kSecAttrKeyTypeRSA as String, "The MonHop signing key is not RSA.")
    try require(attributes?[kSecAttrKeySizeInBits] as? Int == 3072, "The MonHop signing key is not 3072-bit.")
    try require(attributes?[kSecAttrCanSign] as? Bool == true, "The MonHop signing key cannot sign.")
    try require(attributes?[kSecAttrIsExtractable] as? Bool == false, "The MonHop signing key is extractable.")

    try validateCertificate(certificate)
    try validateCodesignAccess(for: privateKey)

    return SigningIdentity(certificate: certificate, fingerprint: fingerprint(of: certificate), loginKeychainPath: loginKeychainPath)
}

private func inspectIdentity() throws -> SigningIdentity? {
    let keychain = try loginKeychain()
    let namedCertificates = try allCertificates(in: keychain.reference).filter { commonName(of: $0) == certificateCommonName }
    if namedCertificates.isEmpty {
        return nil
    }
    try require(namedCertificates.count == 1, "Multiple MonHop signing certificates exist in the login keychain; refusing to choose one.")
    let certificate = namedCertificates[0]
    let certificateFingerprint = fingerprint(of: certificate)

    let identities = try allIdentities(in: keychain.reference)
    let matching = identities.filter { identity in
        guard let candidate = try? identityCertificate(identity) else {
            return false
        }
        return fingerprint(of: candidate) == certificateFingerprint
    }
    try require(matching.count == 1, matching.isEmpty
        ? "The MonHop signing certificate has no matching private key. Refusing to regenerate it."
        : "Multiple MonHop private keys match the signing certificate. Refusing to choose one.")

    return try validateIdentity(matching[0], certificate: certificate, loginKeychainPath: keychain.path)
}

private func run(_ executable: String, _ arguments: [String], input: Data? = nil) throws -> String {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: executable)
    process.arguments = arguments
    let output = Pipe()
    process.standardOutput = output
    process.standardError = output
    var inputPipe: Pipe?
    if input != nil {
        let pipe = Pipe()
        process.standardInput = pipe
        inputPipe = pipe
    }

    try process.run()
    if let input, let inputPipe {
        inputPipe.fileHandleForWriting.write(input)
        try inputPipe.fileHandleForWriting.close()
    }
    process.waitUntilExit()
    let message = String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    guard process.terminationStatus == 0 else {
        throw SigningError.message("\(URL(fileURLWithPath: executable).lastPathComponent) failed: \(message.trimmingCharacters(in: .whitespacesAndNewlines))")
    }
    return message
}

private func randomPassphrase() throws -> String {
    var bytes = [UInt8](repeating: 0, count: 48)
    try requireSuccess(SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes), "Generating a temporary import passphrase")
    return Data(bytes).base64EncodedString()
}

private func setupIdentity() throws -> SigningIdentity {
    let lock = try setupLock()
    return try withExtendedLifetime(lock) {
        if try inspectIdentity() != nil {
            throw SigningError.message("A MonHop signing identity already exists. Setup never replaces an existing identity.")
        }
        let keychain = try loginKeychain()
        try require(try monHopSigningPrivateKeys(in: keychain.reference).isEmpty, "A MonHop-labelled private key already exists without a usable signing identity. Refusing to regenerate it.")

        let temporaryDirectory = FileManager.default.temporaryDirectory
            .appendingPathComponent("monhop-signing-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: temporaryDirectory, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: temporaryDirectory) }

        let keyURL = temporaryDirectory.appendingPathComponent("private-key.pem")
        let certificateURL = temporaryDirectory.appendingPathComponent("certificate.pem")
        let pkcs12URL = temporaryDirectory.appendingPathComponent("identity.p12")
        let passphrase = try randomPassphrase()
        let passphraseLine = Data("\(passphrase)\n".utf8)

        _ = try run(opensslPath, [
            "genrsa", "-aes256", "-passout", "fd:0", "-out", keyURL.path, "3072",
        ], input: passphraseLine)
        _ = try run(opensslPath, [
            "req", "-new", "-x509", "-key", keyURL.path, "-passin", "fd:0", "-out", certificateURL.path,
            "-days", "3650", "-batch", "-sha256", "-subj", "/CN=\(certificateCommonName)",
            "-addext", "basicConstraints=critical,CA:FALSE",
            "-addext", "keyUsage=critical,digitalSignature",
            "-addext", "extendedKeyUsage=codeSigning",
        ], input: passphraseLine)
        let pkcs12Passphrases = Data("\(passphrase)\n\(passphrase)\n".utf8)
        _ = try run(opensslPath, [
            "pkcs12", "-export", "-inkey", keyURL.path, "-passin", "fd:0", "-in", certificateURL.path,
            "-passout", "fd:0", "-out", pkcs12URL.path, "-name", certificateCommonName,
        ], input: pkcs12Passphrases)

        var trustedApplication: SecTrustedApplication?
        try requireSuccess(SecTrustedApplicationCreateFromPath(codesignPath, &trustedApplication), "Restricting signing-key access to codesign")
        guard let trustedApplication else {
            throw SigningError.message("Creating the codesign access rule returned no application.")
        }
        var access: SecAccess?
        try requireSuccess(SecAccessCreate(certificateCommonName as CFString, [trustedApplication] as CFArray, &access), "Creating the signing-key access rule")
        guard let access else {
            throw SigningError.message("Creating the signing-key access rule returned no access rule.")
        }
        try validateCodesignAccess(access)

        let importPassphrase: CFString = passphrase as CFString
        let keyUsage = [kSecAttrCanSign] as CFArray
        let keyAttributes = [kSecAttrIsPermanent, kSecAttrIsSensitive] as CFArray
        var parameters = SecItemImportExportKeyParameters()
        parameters.version = UInt32(SEC_KEY_IMPORT_EXPORT_PARAMS_VERSION)
        parameters.flags = SecKeyImportExportFlags(rawValue: 1)
        parameters.passphrase = Unmanaged.passUnretained(importPassphrase as CFTypeRef)
        parameters.accessRef = Unmanaged.passUnretained(access)
        parameters.keyUsage = Unmanaged.passUnretained(keyUsage)
        parameters.keyAttributes = Unmanaged.passUnretained(keyAttributes)
        let pkcs12Data = try Data(contentsOf: pkcs12URL)
        var imported: CFArray?
        var format = pkcs12Format
        var itemType = aggregateItemType
        try withExtendedLifetime((importPassphrase, keyUsage, keyAttributes, access, pkcs12Data)) {
            try requireSuccess(
                SecItemImport(pkcs12Data as CFData, nil, &format, &itemType, SecItemImportExportFlags(rawValue: 0), &parameters, keychain.reference, &imported),
                "Importing the MonHop signing identity into the login keychain"
            )
        }

        guard let identity = try inspectIdentity() else {
            throw SigningError.message("The MonHop signing identity was not found after import.")
        }
        return identity
    }
}

private func bundleURL(from argument: String) throws -> URL {
    let bundle = URL(fileURLWithPath: argument).resolvingSymlinksInPath()
    try require(bundle.lastPathComponent == "MonHop.app", "Refusing to sign anything except MonHop.app.")
    var directory: ObjCBool = false
    try require(FileManager.default.fileExists(atPath: bundle.path, isDirectory: &directory) && directory.boolValue, "The MonHop.app bundle does not exist.")
    let infoURL = bundle.appendingPathComponent("Contents/Info.plist")
    let infoData = try Data(contentsOf: infoURL)
    let info = try PropertyListSerialization.propertyList(from: infoData, format: nil) as? [String: Any]
    try require(info?["CFBundleIdentifier"] as? String == bundleIdentifier, "The bundle identifier is not \(bundleIdentifier).")
    return bundle
}

private func signerRequirement(for identity: SigningIdentity) -> String {
    "identifier \"\(bundleIdentifier)\" and certificate leaf = H\"\(identity.fingerprint)\""
}

private func signBundle(at bundle: URL) throws {
    guard let identity = try inspectIdentity() else {
        throw SigningError.message("No MonHop signing identity exists. Run setup explicitly; build never creates or falls back to ad-hoc signing.")
    }
    let requirement = signerRequirement(for: identity)
    let entitlements = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("apps/monhop-desktop/macos/entitlements.plist")
    _ = try run(codesignPath, [
        "--force", "--sign", identity.fingerprint, "--keychain", identity.loginKeychainPath,
        "--identifier", bundleIdentifier, "--options", "runtime", "--timestamp=none",
        "--entitlements", entitlements.path,
        "--requirements", "=designated => \(requirement)", bundle.path,
    ])
    _ = try run(codesignPath, ["--verify", "--deep", "--strict", "--verbose=2", bundle.path])
    _ = try run(codesignPath, ["--verify", "--strict", "--verbose=2", "--test-requirement", "=\(requirement)", bundle.path])
    let embeddedRequirements = try run(codesignPath, ["--display", "--requirements", "-", bundle.path])
    try require(embeddedRequirements.contains(requirement), "The bundle does not contain the expected signer-bound designated requirement.")
    print("signed_sha1=\(identity.fingerprint)")
    print("requirement=\(requirement)")
}

private func usage() {
    print("Usage: macos-signing.swift inspect | setup | sign /absolute/path/MonHop.app")
}

do {
    guard CommandLine.arguments.count >= 2 else {
        usage()
        throw SigningError.message("A command is required.")
    }
    switch CommandLine.arguments[1] {
    case "inspect" where CommandLine.arguments.count == 2:
        if let identity = try inspectIdentity() {
            print("status=ready")
            print("sha1=\(identity.fingerprint)")
            print("subject=\(certificateCommonName)")
        } else {
            print("status=missing")
        }
    case "setup" where CommandLine.arguments.count == 2:
        let identity = try setupIdentity()
        print("status=created")
        print("sha1=\(identity.fingerprint)")
        print("subject=\(certificateCommonName)")
    case "sign" where CommandLine.arguments.count == 3:
        try signBundle(at: try bundleURL(from: CommandLine.arguments[2]))
    default:
        usage()
        throw SigningError.message("Invalid arguments.")
    }
} catch {
    FileHandle.standardError.write(Data("macos-signing: \(error)\n".utf8))
    exit(1)
}
