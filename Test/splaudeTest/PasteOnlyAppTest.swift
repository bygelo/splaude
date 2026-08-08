import XCTest

@testable import splaude

/// The one thing standing between a dictation and a screenful of the letter `a`.
///
/// LiveTyper posts every character on virtual key 0, which is `kVK_ANSI_A`, and
/// a remote-desktop client re-encodes what it receives by keycode rather than
/// reading the unicode payload. Classifying the take's app wrong in either
/// direction is expensive: a false negative ships the bug, and a false positive
/// quietly takes live typing away from an app that never needed it.
final class PasteOnlyAppTest: XCTestCase {

    private let custom = ["com.example.myclient"]

    func testABuiltinIdentifierIsPasteOnly() {
        XCTAssertTrue(
            Setting.isPasteOnly("com.microsoft.rdc.macos", within: Setting.builtinPasteOnlyApp)
        )
    }

    func testACustomIdentifierIsPasteOnly() {
        XCTAssertTrue(Setting.isPasteOnly("com.example.myclient", within: custom))
    }

    func testTheComposedListKeepsBothHalves() {
        // Adding your own client must never cost you the shipped list.
        let composed = Setting.builtinPasteOnlyApp + custom
        XCTAssertTrue(Setting.isPasteOnly("com.example.myclient", within: composed))
        XCTAssertTrue(Setting.isPasteOnly("com.citrix.XenAppViewer", within: composed))
    }

    func testAnUnlistedIdentifierTypesLive() {
        // The carve-out is narrow: everything else keeps the existing behaviour.
        let composed = Setting.builtinPasteOnlyApp + custom
        XCTAssertFalse(Setting.isPasteOnly("com.apple.dt.Xcode", within: composed))
        XCTAssertFalse(Setting.isPasteOnly("com.microsoft.VSCode", within: composed))
    }

    func testANilBundleIdentifierTypesLive() {
        // Some processes expose no identifier at all, and an unidentified app is
        // not evidence of a remote-desktop client.
        XCTAssertFalse(Setting.isPasteOnly(nil, within: Setting.builtinPasteOnlyApp))
    }

    func testAnEmptyBundleIdentifierTypesLive() {
        XCTAssertFalse(Setting.isPasteOnly("", within: Setting.builtinPasteOnlyApp))
    }

    func testMatchingIgnoresCase() {
        XCTAssertTrue(Setting.isPasteOnly("com.carriez.rustdesk", within: Setting.builtinPasteOnlyApp))
        XCTAssertTrue(Setting.isPasteOnly("COM.VMWARE.FUSION", within: Setting.builtinPasteOnlyApp))
    }

    func testAnEmptyListMatchesNothing() {
        // Turning the built-in list off has to actually turn it off.
        XCTAssertFalse(Setting.isPasteOnly("com.microsoft.rdc.macos", within: []))
    }

    func testTheBuiltinListIsWellFormed() {
        // A typo'd or blank identifier here would silently never match, which
        // looks exactly like the bug it is supposed to fix.
        for identifier in Setting.builtinPasteOnlyApp {
            XCTAssertFalse(identifier.isEmpty)
            XCTAssertTrue(identifier.contains("."), "\(identifier) is not a bundle identifier")
            XCTAssertEqual(identifier, identifier.trimmingCharacters(in: .whitespaces))
        }
        XCTAssertEqual(Set(Setting.builtinPasteOnlyApp).count, Setting.builtinPasteOnlyApp.count)
    }
}
