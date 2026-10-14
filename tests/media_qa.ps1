# Independent headless media and agent contract QA. Production code stays untouched.
# Uses only INKPAD_DATA_DIR in an isolated, newly-created test workspace.
# No desktop application, window, screen, clipboard, mouse or keyboard access.
param(
    [string]$AgentExe = 'D:\chat\inkpad\target\release\inkpad-agent.exe',
    [string]$OutputRoot = 'D:\chat\inkpad\dist\media-qa-runs'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert([bool]$condition, [string]$message) {
    if (!$condition) { throw "MEDIA QA FAIL: $message" }
}

function New-Session([string]$exe, [string]$data) {
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $exe
    $start.Arguments = 'mcp'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.EnvironmentVariables['INKPAD_DATA_DIR'] = $data
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $start
    Assert ($process.Start()) 'Agent executable failed to start'
    $reader = New-Object System.IO.StreamReader($process.StandardOutput.BaseStream, [System.Text.Encoding]::UTF8)
    return [pscustomobject]@{ Process = $process; Reader = $reader; NextId = 0 }
}

function Close-Session($session) {
    if ($null -eq $session) { return }
    try { $session.Process.StandardInput.Close() } catch {}
    if (!$session.Process.WaitForExit(3000)) {
        # This is an isolated CLI process launched by this QA script.
        $session.Process.Kill()
        [void]$session.Process.WaitForExit(1500)
    }
    $session.Process.Dispose()
}

function Send-Rpc($session, [string]$method, $params = @{}) {
    $session.NextId = $session.NextId + 1
    $request = [ordered]@{
        jsonrpc = '2.0'
        id = [int]$session.NextId
        method = $method
        params = $params
    }
    $line = ConvertTo-Json -InputObject $request -Compress -Depth 64
    # Windows PowerShell 5.1 defaults redirected stdin to the active OEM code
    # page, which corrupts UTF-8 notes. Write exactly RFC 8259 UTF-8 bytes.
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($line + "`n")
    $session.Process.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
    $session.Process.StandardInput.BaseStream.Flush()
    return [int]$request.id
}

function Receive-Rpc($session, [int]$id, [string]$operation) {
    $task = $session.Reader.ReadLineAsync()
    Assert ($task.Wait(15000)) "Timed out waiting for JSON-RPC response to $operation"
    Assert ($null -ne $task.Result) "Agent closed stdout during $operation"
    $reply = ConvertFrom-Json -InputObject $task.Result
    Assert ($reply.id -eq $id) "Wrong response ID for $operation"
    if ($null -ne $reply.PSObject.Properties['error']) {
        throw "JSON-RPC $operation failed: $($reply.error.message)"
    }
    return $reply.result
}

function Invoke-Rpc($session, [string]$method, $params = @{}) {
    $id = Send-Rpc $session $method $params
    return Receive-Rpc $session $id $method
}

function Invoke-Tool($session, [string]$name, $arguments = @{}, [switch]$ExpectError) {
    $response = Invoke-Rpc $session 'tools/call' ([ordered]@{ name = $name; arguments = $arguments })
    if ($ExpectError) {
        Assert ([bool]$response.isError) "$name unexpectedly accepted invalid request"
        return $null
    }
    Assert (!$response.isError) "$name rejected request: $($response.content[0].text)"
    return $response.structuredContent
}

function Save-Fixtures([string]$dir) {
    Add-Type -AssemblyName System.Drawing
    $bitmap = New-Object System.Drawing.Bitmap(80, 40)
    try {
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.Clear([System.Drawing.Color]::White)
            $graphics.FillRectangle([System.Drawing.Brushes]::DarkBlue, 0, 0, 30, 40)
        } finally { $graphics.Dispose() }
        foreach ($format in @('Png', 'Jpeg', 'Gif')) {
            $extension = if ($format -eq 'Jpeg') { 'jpg' } else { $format.ToLowerInvariant() }
            $path = Join-Path $dir "valid.$extension"
            $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::$format)
        }
    } finally { $bitmap.Dispose() }
    [System.IO.File]::WriteAllBytes((Join-Path $dir 'broken.png'), [byte[]](137,80,78,71,13,10,26,10,0,0))
    [System.IO.File]::WriteAllBytes((Join-Path $dir 'broken.jpg'), [byte[]](255,216,255,224,0,16))
    [System.IO.File]::WriteAllBytes((Join-Path $dir 'broken.gif'), [System.Text.Encoding]::ASCII.GetBytes('GIF89a'))
    [System.IO.File]::WriteAllBytes((Join-Path $dir 'broken.webp'), [System.Text.Encoding]::ASCII.GetBytes('RIFF0000WEBPVP8 '))
    # Embedded Pillow-produced 3x2 lossless WebP, to avoid a runtime image codec dependency.
    [System.IO.File]::WriteAllBytes((Join-Path $dir 'valid.webp'),
        [Convert]::FromBase64String('UklGRhwAAABXRUJQVlA4TA8AAAAvAkAAAAcQ/Y/+ByKi/wEA'))
    [System.IO.File]::WriteAllText((Join-Path $dir 'not-an-image.png'), '<script>alert("not an image")</script>')
    # One byte beyond the documented 12 MiB file cap; source bytes are never
    # decoded or copied, so this validates admission separately from magic.
    [System.IO.File]::WriteAllBytes((Join-Path $dir 'oversized.png'), (New-Object byte[] (12 * 1024 * 1024 + 1)))
}

function Workspace-Snapshot([string]$dataPath) {
    $path = Join-Path $dataPath 'workspace.json'
    Assert (Test-Path -LiteralPath $path) 'Missing workspace.json'
    return Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
}

function Get-Images($value) {
    if ($value -is [array]) { return ,@($value) }
    foreach ($key in @('images', 'media', 'items')) {
        if ($value.PSObject.Properties[$key]) { return ,@($value.$key) }
    }
    throw 'image_list response did not include ordered images/media/items'
}

function Get-ImageId($value) {
    foreach ($key in @('image_id', 'id')) {
        if ($value.PSObject.Properties[$key] -and $value.$key) { return [string]$value.$key }
    }
    if ($value.PSObject.Properties['image'] -and $value.image.id) { return [string]$value.image.id }
    throw 'Image insertion did not return an opaque image ID'
}

$runDir = Join-Path $OutputRoot ([Guid]::NewGuid().ToString('N'))
$dataDir = Join-Path $runDir 'data'
$fixtures = Join-Path $runDir 'fixtures'
$exportDir = Join-Path $runDir 'exports'
New-Item -ItemType Directory -Force -Path $dataDir, $fixtures, $exportDir | Out-Null
Assert (Test-Path -LiteralPath $AgentExe -PathType Leaf) "Missing headless agent executable: $AgentExe"
Save-Fixtures $fixtures
$first = $null
$second = $null
$passed = 0
try {
    $first = New-Session $AgentExe $dataDir
    $initialize = Invoke-Rpc $first 'initialize' @{ protocolVersion = '2025-06-18'; capabilities = @{}; clientInfo = @{ name = 'inkpad-media-qa'; version = '1.0' } }
    Assert ($initialize.serverInfo.name -eq 'inkpad-agent') 'Incorrect MCP identity'
    $toolList = @( (Invoke-Rpc $first 'tools/list').tools )
    $names = @($toolList | ForEach-Object { $_.name })
    Assert ($names -contains 'create_note' -and $names -contains 'read_note') 'Missing core agent tools'
    $payload = '<h1>&"''🌍''</h1><div>literal</div>'
    $created = Invoke-Tool $first 'create_note' ([ordered]@{ title = 'Media QA <>&"'; body = $payload })
    $noteId = [string]$created.id
    Assert (![string]::IsNullOrWhiteSpace($noteId)) 'Create note omitted ID'
    $second = New-Session $AgentExe $dataDir
    $read = Invoke-Tool $second 'read_note' @{ id = $noteId }
    Assert ($read.body -eq $payload) 'Headless concurrent reader lost Unicode/HTML text'
    $passed++
    Write-Output 'PASS headless MCP create/read from two processes with isolated data'

    # Discover the actual media tools through MCP rather than guessing CLI
    # switches or relying on the private Rust implementation.
    $mediaTools = @($names | Where-Object { $_ -match 'image|media|picture|attachment' })
    if ($mediaTools.Count -eq 0) {
        throw "MEDIA CONTRACT NOT IMPLEMENTED: tool list exposes $($names.Count) operations and no image/media API. Pending prime/worker-2 implementation."
    }
    Write-Output "MEDIA TOOL DISCOVERY: $($mediaTools -join ', ')"
    $schema = $toolList | Where-Object { $_.name -in $mediaTools }
    $schema | ConvertTo-Json -Depth 35 | Set-Content -LiteralPath (Join-Path $runDir 'media-tool-schema.json') -Encoding UTF8
    $required = @('image_insert_file','image_insert_base64','image_list','image_resize','image_move','image_caption','image_delete','image_export_html','read_image','export_image')
    foreach ($tool in $required) { Assert ($names -contains $tool) "Missing required media parity tool $tool" }
    $passed++
    Write-Output 'PASS all ten picture MCP tools discoverable'

    # Permission and path admission checks must not touch the workspace.
    $pngFile = Join-Path $fixtures 'valid.png'
    $sourceHash = (Get-FileHash -LiteralPath $pngFile -Algorithm SHA256).Hash
    Invoke-Tool $first 'image_insert_file' @{ id = $noteId; path = $pngFile } -ExpectError | Out-Null
    Invoke-Tool $first 'image_insert_file' @{ id = $noteId; path = '.\valid.png'; user_approved = $true } -ExpectError | Out-Null
    $withTraversal = Join-Path $fixtures '..\fixtures\valid.png'
    Invoke-Tool $first 'image_insert_file' @{ id = $noteId; path = $withTraversal; user_approved = $true } -ExpectError | Out-Null
    foreach ($badFile in @('broken.png','broken.jpg','broken.gif','broken.webp','not-an-image.png','oversized.png')) {
        $path = Join-Path $fixtures $badFile
        Invoke-Tool $first 'image_insert_file' @{ id = $noteId; path = $path; user_approved = $true } -ExpectError | Out-Null
    }
    $invalidList = Get-Images (Invoke-Tool $second 'image_list' @{ id = $noteId })
    Assert ($invalidList.Count -eq 0) 'Rejected picture input altered note images'
    $passed++
    Write-Output 'PASS missing consent, traversal, invalid formats, oversized input are rejected without mutation'

    $allIds = New-Object System.Collections.Generic.List[string]
    foreach ($extension in @('png','jpg','gif','webp')) {
        $path = Join-Path $fixtures "valid.$extension"
        $beforeHash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
        $result = Invoke-Tool $first 'image_insert_file' @{ id = $noteId; path = $path; user_approved = $true }
        $newId = Get-ImageId $result
        Assert ($newId -match '^[A-Za-z0-9-]+$') 'Imported image ID is not opaque/safe'
        $allIds.Add($newId)
        Assert ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -eq $beforeHash) "Import altered $extension source file"
        Assert ((Get-FileHash -LiteralPath $pngFile -Algorithm SHA256).Hash -eq $sourceHash) 'Picture import changed source file'
    }
    $base64 = [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($pngFile))
    Invoke-Tool $first 'image_insert_base64' @{ id = $noteId; data_base64 = $base64 } -ExpectError | Out-Null
    Invoke-Tool $first 'image_insert_base64' @{ id = $noteId; data_base64 = '!!!!'; user_approved = $true } -ExpectError | Out-Null
    $inline = Invoke-Tool $second 'image_insert_base64' @{ id = $noteId; data_base64 = $base64; user_approved = $true }
    $allIds.Add((Get-ImageId $inline))
    $images = Get-Images (Invoke-Tool $second 'image_list' @{ id = $noteId })
    Assert ($images.Count -eq 5) 'Supported image inserts did not produce five ordered images'
    foreach ($item in $images) {
        Assert ([int]$item.width_percent -ge 10 -and [int]$item.width_percent -le 100) 'Stored image width outside valid display range'
        Assert ([string]$item.filename -notmatch '\\|/|\.\.') 'Untrusted path in image metadata'
    }
    $passed++
    Write-Output 'PASS valid PNG/JPEG/GIF/WebP file imports and inline base64, source unchanged, metadata opaque'

    # An AI must be able to understand a chosen picture without unrestricted
    # filesystem access or leaking duplicate base64 into structured metadata.
    $inspectedId = $allIds[0]
    Invoke-Tool $first 'read_image' @{ id = $noteId; image_id = $inspectedId } -ExpectError | Out-Null
    $imageReply = Invoke-Rpc $first 'tools/call' @{
        name = 'read_image'; arguments = @{
            id = $noteId; image_id = $inspectedId; user_approved = $true
        }
    }
    Assert (!$imageReply.isError) 'Approved read_image unexpectedly refused selected picture'
    Assert ($imageReply.content[0].type -eq 'image' -and $imageReply.content[0].mimeType -eq 'image/png') 'read_image did not return standard MCP image content'
    Assert ($null -eq $imageReply.structuredContent.PSObject.Properties['data_base64']) 'Private image bytes were duplicated into metadata response'
    $decodedImage = [Convert]::FromBase64String([string]$imageReply.content[0].data)
    Assert ($decodedImage.Length -gt 100 -and $decodedImage[0] -eq 137 -and $decodedImage[1] -eq 80) 'MCP image block was not valid PNG bytes'
    $singleExport = Join-Path $exportDir 'single-picture.png'
    Invoke-Tool $first 'export_image' @{ id = $noteId; image_id = $inspectedId; path = $singleExport } -ExpectError | Out-Null
    $null = Invoke-Tool $second 'export_image' @{
        id = $noteId; image_id = $inspectedId; path = $singleExport; user_approved = $true
    }
    Assert (Test-Path -LiteralPath $singleExport -PathType Leaf) 'Approved single image export did not create output'
    Assert ((Get-FileHash -LiteralPath $singleExport -Algorithm SHA256).Hash -eq
        (Get-FileHash -LiteralPath (Join-Path $dataDir ('media\' + $inspectedId + '.png')) -Algorithm SHA256).Hash) 'Exported image bytes differ from stored attachment'
    Invoke-Tool $second 'export_image' @{ id = $noteId; image_id = $inspectedId; path = $singleExport; user_approved = $true } -ExpectError | Out-Null
    Invoke-Tool $second 'export_image' @{ id = $noteId; image_id = $inspectedId; path = (Join-Path $exportDir 'bad.jpg'); user_approved = $true } -ExpectError | Out-Null
    $passed++
    Write-Output 'PASS approved MCP image understanding block, no duplicate bytes, safe single PNG export and no overwrite'

    $resizeSchema = @($toolList | Where-Object { $_.name -eq 'image_resize' })[0].inputSchema.properties.width_percent
    $minWidth = [int]$resizeSchema.minimum
    $maxWidth = [int]$resizeSchema.maximum
    Assert ($minWidth -ge 10 -and $minWidth -le 50 -and $maxWidth -eq 100) 'Image schema published an unreasonable resize range'
    $targetImage = $allIds[4]
    foreach ($illegalWidth in @(-1,0,($minWidth - 1),($maxWidth + 1),250)) {
        Invoke-Tool $second 'image_resize' @{ id = $noteId; image_id = $targetImage; width_percent = $illegalWidth } -ExpectError | Out-Null
    }
    foreach ($validWidth in @($minWidth,33,$maxWidth)) {
        $null = Invoke-Tool $second 'image_resize' @{ id = $noteId; image_id = $targetImage; width_percent = $validWidth }
        $seen = Get-Images (Invoke-Tool $first 'image_list' @{ id = $noteId })
        $matching = @($seen | Where-Object { $_.id -eq $targetImage })
        Assert ($matching.Count -eq 1 -and [int]$matching[0].width_percent -eq $validWidth) "Image resizing to $validWidth% was not persisted"
    }
    $cap = 'Caption <b onclick="evil">&"' + "'"
    $null = Invoke-Tool $first 'image_caption' @{ id = $noteId; image_id = $targetImage; caption = $cap }
    Invoke-Tool $first 'image_caption' @{ id = $noteId; image_id = $targetImage; caption = ('X'*501) } -ExpectError | Out-Null
    $null = Invoke-Tool $second 'image_move' @{ id = $noteId; image_id = $targetImage; position = 0 }
    $moved = Get-Images (Invoke-Tool $first 'image_list' @{ id = $noteId })
    Assert ($moved[0].id -eq $targetImage -and $moved[0].caption -eq $cap) 'Image caption or move-to-front failed'
    foreach ($i in 0..($moved.Count-1)) { Assert ([int]$moved[$i].position -eq $i) 'Noncontiguous stored image positions' }
    Invoke-Tool $first 'image_move' @{ id = $noteId; image_id = $targetImage; position = 4097 } -ExpectError | Out-Null
    $passed++
    Write-Output 'PASS resize boundaries, caption escaping input, ordered move and metadata consistency'

    $htmlPath = Join-Path $exportDir 'private-image-note.html'
    Invoke-Tool $first 'image_export_html' @{ id = $noteId; path = $htmlPath } -ExpectError | Out-Null
    $null = Invoke-Tool $first 'image_export_html' @{ id = $noteId; path = $htmlPath; user_approved = $true }
    Assert (Test-Path -LiteralPath $htmlPath -PathType Leaf) 'HTML export missing'
    $html = [System.IO.File]::ReadAllText($htmlPath)
    Assert ($html.Contains('data:image/png;base64,')) 'HTML export lacks embedded self-contained media'
    Assert ($html.Contains('&lt;h1&gt;&amp;&quot;')) 'HTML body was not entity-escaped'
    Assert (!$html.Contains('<h1>&"')) 'HTML export leaked unescaped body injection'
    Assert ($html.Contains('Caption &lt;b onclick=&quot;evil&quot;&gt;&amp;&quot;&#39;')) 'HTML caption was not escaped'
    Assert (!$html.Contains($runDir) -and !$html.Contains($dataDir)) 'HTML export disclosed private absolute paths'
    Assert ($html.Contains('Content-Security-Policy')) 'HTML export missing restrictive content policy'
    Assert ($html.Contains('width:100%')) 'HTML export failed to preserve resized display width'
    $beforeExportHash = (Get-FileHash -LiteralPath $htmlPath -Algorithm SHA256).Hash
    Invoke-Tool $second 'image_export_html' @{ id = $noteId; path = $htmlPath; user_approved = $true } -ExpectError | Out-Null
    Assert ((Get-FileHash -LiteralPath $htmlPath -Algorithm SHA256).Hash -eq $beforeExportHash) 'Refused overwrite modified existing HTML'
    $passed++
    Write-Output 'PASS self-contained escaped HTML, CSP, caption, proportions, no private path disclosure, no overwrite'

    # Restart both MCP agents: no GUI process is launched. Verify stored refs,
    # managed PNG dimensions/aspect and preservation of original body text.
    Close-Session $second; $second = $null
    Close-Session $first; $first = $null
    $first = New-Session $AgentExe $dataDir
    $second = New-Session $AgentExe $dataDir
    $persisted = Get-Images (Invoke-Tool $first 'image_list' @{ id = $noteId })
    Assert ($persisted.Count -eq 5 -and $persisted[0].id -eq $targetImage) 'Image order was lost on agent restart'
    Assert ((Invoke-Tool $second 'read_note' @{ id = $noteId }).body -eq $payload) 'Pictures changed page body'
    $managed = Join-Path $dataDir 'media'
    Assert (Test-Path -LiteralPath $managed -PathType Container) 'Missing private asset directory'
    $pngAssets = @(Get-ChildItem -LiteralPath $managed -Filter '*.png' -File)
    Assert ($pngAssets.Count -ge 5) 'Missing managed copies after successful imports'
    $pngSizes = New-Object System.Collections.Generic.List[string]
    foreach ($asset in $pngAssets) {
        $image = [System.Drawing.Image]::FromFile($asset.FullName)
        try { $pngSizes.Add("$($image.Width)x$($image.Height)") }
        finally { $image.Dispose() }
    }
    Assert ($pngSizes -contains '80x40' -and $pngSizes -contains '3x2') 'Normalization lost source image aspect ratios'
    $passed++
    Write-Output 'PASS restart persistence, opaque managed PNG copies and source aspect ratio'

    Invoke-Tool $first 'image_delete' @{ id = $noteId; image_id = $targetImage } -ExpectError | Out-Null
    $null = Invoke-Tool $second 'image_delete' @{ id = $noteId; image_id = $targetImage; confirm = $true }
    $afterImageDelete = Get-Images (Invoke-Tool $first 'image_list' @{ id = $noteId })
    Assert ($afterImageDelete.Count -eq 4 -and !(@($afterImageDelete | Where-Object { $_.id -eq $targetImage }).Count)) 'Image delete left active metadata references'
    Assert ((Invoke-Tool $first 'read_note' @{ id = $noteId }).body -eq $payload) 'Image delete damaged text content'
    $passed++
    Write-Output 'PASS image-specific delete confirmation, metadata and text preservation'

    # Simultaneous requests to two persistent stdio MCP processes exercise
    # cross-process read/modify/write locking without opening the GUI.
    foreach ($i in 1..12) {
        $firstMark = "MCP-A-$i"
        $secondMark = "MCP-B-$i"
        $a = Send-Rpc $first 'tools/call' @{ name = 'append_to_note'; arguments = @{ id = $noteId; text = $firstMark } }
        $b = Send-Rpc $second 'tools/call' @{ name = 'append_to_note'; arguments = @{ id = $noteId; text = $secondMark } }
        $r1 = Receive-Rpc $first $a 'concurrent append A'
        $r2 = Receive-Rpc $second $b 'concurrent append B'
        Assert (!$r1.isError -and !$r2.isError) "Cross-process simultaneous append failed round $i"
    }
    $concurrentText = [string](Invoke-Tool $second 'read_note' @{ id = $noteId }).body
    foreach ($i in 1..12) {
        foreach ($token in @("MCP-A-$i", "MCP-B-$i")) {
            Assert ($concurrentText.Contains($token)) "Concurrent MCP mutation lost marker $token"
        }
    }
    Assert ((Get-Images (Invoke-Tool $first 'image_list' @{ id = $noteId })).Count -eq 4) 'Concurrent text changes corrupted image references'
    $passed++
    Write-Output 'PASS simultaneous writes from two independent headless MCP agents preserve both text and media'

    # Deleting a test note is authorised within this isolated QA workspace.
    $null = Invoke-Tool $first 'delete_note' @{ id = $noteId; confirm = $true }
    $noteStillExists = @((Workspace-Snapshot $dataDir).notes | Where-Object { $_.id -eq $noteId })
    Assert ($noteStillExists.Count -eq 0) 'Deleted note remains in active workspace'
    $passed++
    Write-Output 'PASS isolated note deletion removes all active image references'

    # An oversized single JSON-RPC line should fail with a bounded parse error
    # and leave the long-lived stdio process usable for the next request.
    $tooLarge = '{"jsonrpc":"2.0","id":10000,"method":"tools/call","params":{"name":"create_note","arguments":{"title":"Big","body":"' +
        ('x' * (2 * 1024 * 1024 + 1024)) + '"}}}'
    $oversizedBytes = [System.Text.Encoding]::UTF8.GetBytes($tooLarge + "`n")
    $first.Process.StandardInput.BaseStream.Write($oversizedBytes, 0, $oversizedBytes.Length)
    $first.Process.StandardInput.BaseStream.Flush()
    $overReply = $first.Reader.ReadLineAsync()
    Assert ($overReply.Wait(15000)) 'Oversized JSON-RPC message stalled the agent'
    $parseResponse = ConvertFrom-Json -InputObject $overReply.Result
    Assert ($parseResponse.error.code -eq -32700) 'Oversized RPC payload did not return a parse error'
    $ping = Invoke-Rpc $first 'ping' @{}
    Assert ($null -ne $ping) 'Oversize rejected request broke subsequent MCP request handling'
    $passed++
    Write-Output 'PASS >2 MiB RPC message rejected without killing persistent agent'

    # New backup contains a valid note with an attached image. The primary is
    # deliberately corrupted only inside this disposable INKPAD_DATA_DIR.
    $recovery = Invoke-Tool $first 'create_note' @{ title = 'Recovery < &'; body = 'Backed up with image' }
    $recoveryId = [string]$recovery.id
    $recoveryImage = Invoke-Tool $second 'image_insert_file' @{
        id = $recoveryId; path = $pngFile; user_approved = $true
    }
    $recoveryImageId = Get-ImageId $recoveryImage
    $null = Invoke-Tool $second 'image_caption' @{ id = $recoveryId; image_id = $recoveryImageId; caption = 'still backed up' }
    $workspaceFile = Join-Path $dataDir 'workspace.json'
    $backupFile = Join-Path $dataDir 'workspace.json.bak'
    Assert (Test-Path -LiteralPath $backupFile -PathType Leaf) 'Workspace backup was not created'
    $backupBytes = [System.IO.File]::ReadAllBytes($backupFile)
    $backupJson = [System.Text.Encoding]::UTF8.GetString($backupBytes) | ConvertFrom-Json
    $backupRecovered = @($backupJson.notes | Where-Object { $_.id -eq $recoveryId })
    Assert ($backupRecovered.Count -eq 1 -and @($backupRecovered[0].images).Count -ge 1) 'Backup lacks media references before corruption'
    [System.IO.File]::WriteAllText($workspaceFile, '{"notes":"TRUNCATED')
    $fromBackup = Get-Images (Invoke-Tool $first 'image_list' @{ id = $recoveryId })
    Assert ($fromBackup.Count -ge 1 -and $fromBackup[0].id -eq $recoveryImageId) 'Corrupt primary did not recover picture metadata from valid backup'
    Assert ([System.IO.File]::ReadAllText($workspaceFile) -eq '{"notes":"TRUNCATED') 'Recovery overwrote corrupt primary without confirmation'
    Assert ([Convert]::ToBase64String([System.IO.File]::ReadAllBytes($backupFile)) -eq [Convert]::ToBase64String($backupBytes)) 'Recovery modified the last valid backup'
    $passed++
    Write-Output 'PASS corrupt-primary fallback preserves backup and image references without overwriting damaged original'

    # GC must reclaim old images automatically once BOTH the active workspace
    # and its previous-save backup no longer reference them. In contrast, a
    # newly deleted recovery image must remain while .bak still references it.
    $referencedIds = @($backupJson.notes | ForEach-Object { @($_.images | ForEach-Object { $_.id }) })
    $remainingFiles = @(Get-ChildItem -LiteralPath $managed -Filter '*.png' -File)
    $orphanFiles = @($remainingFiles | Where-Object { $_.BaseName -notin $referencedIds })
    Assert ($orphanFiles.Count -eq 0) "Stale deleted images were not automatically reclaimed after backup rotation (orphan count $($orphanFiles.Count))"
    [System.IO.File]::Copy($backupFile, $workspaceFile, $true)
    $null = Invoke-Tool $first 'image_delete' @{ id = $recoveryId; image_id = $recoveryImageId; confirm = $true }
    $recoveryPNG = Join-Path $managed ($recoveryImageId + '.png')
    Assert (Test-Path -LiteralPath $recoveryPNG -PathType Leaf) 'Cleanup prematurely erased image still needed by previous-save backup'
    $null = Invoke-Tool $second 'append_to_note' @{ id = $recoveryId; text = 'rotates previous-save backup' }
    Assert (!(Test-Path -LiteralPath $recoveryPNG -PathType Leaf)) 'Deleted image asset still on disk after backup rotation removed final reference'
    $passed++
    Write-Output 'PASS deleted image reclaimed automatically after backup rotation, recovery references protected'
} catch {
    Write-Output $_.Exception.Message
    throw
} finally {
    Close-Session $second
    Close-Session $first
    Write-Output "MEDIA_QA_PASSED_STEPS=$passed"
    Write-Output "MEDIA_QA_RUN=$runDir"
    # Preserve isolated evidence for diagnosing regressions.
}
