# usb-mass-storage Specification

## Purpose

Lets the desktop host identify and read a USB flash drive or card reader attached to the
phone, and measure its read speed, as a first real use of USB device access.

## Requirements

### Requirement: Only Bulk-Only mass-storage devices are used
The host SHALL accept a device for mass storage only if it has an interface of class 0x08
with protocol 0x50 (Bulk-Only) and one bulk IN and one bulk OUT endpoint; otherwise it
SHALL refuse with a clear message and not open the device.

#### Scenario: Not a drive
- **WHEN** the user runs `hubbridge usb storage <id>` for a USB-UART adapter
- **THEN** the command fails saying the device is not a USB mass-storage device

### Requirement: Identify the drive
`hubbridge usb storage <id>` SHALL print the drive's SCSI INQUIRY vendor, product,
revision and whether it is removable, and its capacity from READ CAPACITY.

#### Scenario: Flash drive on the phone's hub
- **WHEN** the user runs `hubbridge usb storage 1004` for a flash drive with permission granted
- **THEN** it prints the vendor/product/revision line and the capacity in GB with block count and block size

### Requirement: Reading is read-only
The mass-storage client SHALL send only read and status commands (TEST UNIT READY,
REQUEST SENSE, INQUIRY, READ CAPACITY(10), READ(10)) and never a command that writes.

#### Scenario: Speed test leaves the drive unchanged
- **WHEN** the user runs a speed test on a drive
- **THEN** no write command is sent to the drive

### Requirement: Read data is correct and complete
A READ(10) SHALL return exactly the requested blocks in order, or fail with an error; a
failed command SHALL not break later commands.

#### Scenario: Read larger than one transfer
- **WHEN** the host reads 300 blocks of 512 bytes in one command
- **THEN** it gets 153600 bytes equal to the drive's content of those blocks, in order

#### Scenario: Read past the end
- **WHEN** a READ(10) asks for blocks past the last one
- **THEN** that read fails with an error and the next valid read succeeds

### Requirement: Speed measurement
With `--bench <MiB>` the command SHALL read that many MiB sequentially from the start of the
drive and print bytes, time, MB/s and the link type; `--chunk-kib` and `--command-kib`
SHALL set the transfer and command sizes.

#### Scenario: Benchmark over Wi-Fi
- **WHEN** the user runs `hubbridge usb storage 1004 --bench 32` over Wi-Fi
- **THEN** it prints "read 32.0 MiB in <s> s: <MB/s> MB/s over Wi-Fi/LAN"

### Requirement: The drive goes back to the phone
When the command ends, successfully or not, the host SHALL release the interface and close
the device so the phone can use the drive again.

#### Scenario: After the command
- **WHEN** `hubbridge usb storage` finishes
- **THEN** the device is closed on the agent and can be opened again by another command

### Requirement: Live descriptors
`hubbridge usb descriptors <id>` SHALL read the device and configuration descriptors and
the string descriptors from the device itself with control transfers, print them, and say
whether they match the copy Android reported.

#### Scenario: Descriptors match
- **WHEN** the user runs `hubbridge usb descriptors 1004`
- **THEN** it prints the parsed descriptors, the manufacturer/product/serial strings and "identical to what Android reported"

#### Scenario: No permission
- **WHEN** the device has no USB permission on the phone
- **THEN** the command fails and tells the user to run `hubbridge grant <id>` and tap Allow
