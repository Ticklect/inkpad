# Independent headless rich-text / persistence QA.
# Never starts Inkpad's GUI. All mutations are confined to a UUID workspace.
param(
    [string]$AgentExe = 'D:\chat\inkpad\target\release\inkpad-agent.exe',
    [string]$OutputRoot = 'D:\chat\inkpad\dist\rich-text-qa-runs',
    [switch]$RequireRich,
    [switch]$RequireStyledHtml,
    [switch]$RequireSelectionMcp
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
function Check([bool]$condition, [string]$label) {
    if (!$condition) { throw "FAIL $label" }
}
function Session([string]$exe, [string]$workspace) {
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $exe
    $start.Arguments = 'mcp'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.EnvironmentVariables['INKPAD_DATA_DIR'] = $workspace
    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $start
    Check ($proc.Start()) 'agent start'
    $reader = New-Object System.IO.StreamReader($proc.StandardOutput.BaseStream, [System.Text.Encoding]::UTF8)
    return [pscustomobject]@{ P = $proc; Reader = $reader; Id = 0 }
}
function Close-Session($s) {
    if ($null -eq $s) { return }
    try { $s.P.StandardInput.Close() } catch {}
    if (!$s.P.WaitForExit(3000)) {
        $s.P.Kill()
        [void]$s.P.WaitForExit(1500)
    }
    $s.Reader.Dispose()
    $s.P.Dispose()
}
function Send($s, [string]$method, $params) {
    $s.Id = [int]$s.Id + 1
    $request = [ordered]@{
        jsonrpc = '2.0'; id = [int]$s.Id; method = $method; params = $params
    }
    $payload = (ConvertTo-Json -InputObject $request -Depth 64 -Compress) + "`n"
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
    $s.P.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
    $s.P.StandardInput.BaseStream.Flush()
    return [int]$s.Id
}
function Receive($s, [int]$id, [string]$context) {
    $task = $s.Reader.ReadLineAsync()
    Check ($task.Wait(15000)) "timeout $context"
    Check ($null -ne $task.Result) "stdout closed $context"
    $response = ConvertFrom-Json -InputObject $task.Result
    Check ($response.id -eq $id) "wrong response id $context"
    Check ($null -eq $response.PSObject.Properties['error']) "RPC error $context"
    return $response.result
}
function RPC($s, [string]$method, $params = @{}) {
    $id = Send $s $method $params
    return Receive $s $id $method
}
function Tool($s, [string]$name, $arguments = @{}, [switch]$ExpectError) {
    $result = RPC $s 'tools/call' @{ name = $name; arguments = $arguments }
    if ($ExpectError) {
        Check ([bool]$result.isError) "$name accepted invalid input"
        return $null
    }
    Check (!$result.isError) "$name rejected $($result.content[0].text)"
    return $result.structuredContent
}
function Read-Workspace([string]$dir) {
    return [System.IO.File]::ReadAllText((Join-Path $dir 'workspace.json'), [System.Text.Encoding]::UTF8) | ConvertFrom-Json
}
function Equal-Bytes([string]$a, [string]$b) {
    return ((Get-FileHash -LiteralPath $a -Algorithm SHA256).Hash -eq (Get-FileHash -LiteralPath $b -Algorithm SHA256).Hash)
}
function Text-RichFields($note) {
    $found = @()
    foreach($key in @('rich_text','rich_spans','format_spans','style_ranges','text_runs','formatting','marks','spans')) {
        if ($note.PSObject.Properties[$key]) { $found += $key }
    }
    return $found
}
function Style-At($runs, [int]$position) {
    foreach($run in @($runs)) {
        if ([int]$run.start -le $position -and $position -lt [int]$run.end) { return $run.style }
    }
    return [pscustomobject]@{ bold = $false; color = $null; size = $null; font_family = $null }
}
function Check-Runs($runs, [int]$length) {
    $previous = 0
    $last = $null
    foreach($run in @($runs)) {
        Check ($run.start -ge $previous -and $run.end -gt $run.start -and $run.end -le $length) 'invalid overlapping/out-of-bounds rich runs'
        $now = $run.style | ConvertTo-Json -Compress -Depth 10
        Check (!($previous -eq [int]$run.start -and $now -eq $last)) 'adjacent equal rich runs not normalized'
        $previous = [int]$run.end
        $last = $now
    }
}

Check (Test-Path -LiteralPath $AgentExe -PathType Leaf) "Missing agent $AgentExe"
$run = Join-Path $OutputRoot ([Guid]::NewGuid().ToString('N'))
$data = Join-Path $run 'notebook'
$out = Join-Path $run 'exports'
New-Item -ItemType Directory -Path $data,$out -Force | Out-Null
$s1 = $null
$s2 = $null
$pass = 0
try {
    $s1 = Session $AgentExe $data
    $init = RPC $s1 'initialize' @{ protocolVersion = '2025-06-18'; clientInfo = @{name='rich-text-qa';version='1'};capabilities=@{} }
    Check ($init.serverInfo.name -eq 'inkpad-agent') 'wrong server'
    $tools = @((RPC $s1 'tools/list').tools)
    $toolNames = @($tools | ForEach-Object { $_.name })
    foreach($needed in @('create_note','read_note','update_note','append_to_note','add_stroke','set_font_size','set_font_choice')) {
        Check ($toolNames -contains $needed) "missing basic MCP $needed"
    }
    $s2 = Session $AgentExe $data
    # PowerShell 5.1 does not support `u{} escape; encode a combining accent explicitly.
    $unicode = 'A🏳️‍🌈e' + [char]0x0301 + ' 中 हिन्दी العربية אבג Русский 👩‍🚀 ＆' + "`nsecond line"
    $title = '混合 Unicode 🧪 & <tag> "quotes"'
    $expectedBody = $unicode
    $created = Tool $s1 'create_note' @{title=$title;body=$unicode}
    $noteId = [string]$created.id
    Check (![string]::IsNullOrWhiteSpace($noteId)) 'create missing id'
    $a = Tool $s2 'read_note' @{ id=$noteId }
    Check ($a.body -ceq $unicode -and $a.title -ceq $title) 'multiscript Unicode or combining accents changed'
    $pass++
    Write-Output 'PASS multiscript mixed-direction Unicode, emojis, combining marks and newlines roundtrip two MCP processes'

    $stroke = Tool $s1 'add_stroke' @{id=$noteId;width=5;color='#123ABC';points=@(@(1,2),@(3,4))}
    Check ($stroke.stroke_count -eq 1) 'stroke fixture missing'
    # Known-valid 3x2 WebP lossless fixture; Inkpad imports and re-encodes PNG.
    $webpBase64 = 'UklGRhwAAABXRUJQVlA4TA8AAAAvAkAAAAcQ/Y/+ByKi/wEA'
    $embedded = Tool $s1 'image_insert_base64' @{id=$noteId; data_base64=$webpBase64;user_approved=$true}
    Check (![string]::IsNullOrWhiteSpace([string]$embedded.image.id)) 'WebP picture fixture import failed'
    $after = Tool $s2 'read_note' @{id=$noteId}
    Check ($after.strokes.Count -eq 1 -and $after.images.Count -eq 1 -and $after.body -ceq $unicode) 'picture/drawing mutation discarded text'
    $pass++
    Write-Output 'PASS drawing and picture additions preserve Unicode body'

    # Independent agent updates MUST preserve all note-adjacent metadata.
    $original = Tool $s1 'read_note' @{id=$noteId}
    $font = Tool $s2 'set_font_choice' @{id=$noteId;choice='Serif'}
    $size = Tool $s1 'set_font_size' @{id=$noteId;size=24}
    $updated = Tool $s2 'update_note' @{id=$noteId;title=($title+' edited')}
    $again = Tool $s1 'read_note' @{id=$noteId}
    Check ($again.body -ceq $unicode -and $again.strokes.Count -eq 1 -and $again.images.Count -eq 1 -and $again.font_size -eq 24 -and $again.font_choice -eq 'Serif') 'unrelated edits lost note data'
    $pass++
    Write-Output 'PASS independent font/title mutations retain Unicode text and drawings'

    $richNames = @('get_note_formatting','format_text_range','clear_text_formatting','replace_text_range')
    $richAvailable = @($richNames | Where-Object { $_ -notin $toolNames }).Count -eq 0
    if ($RequireRich -and !$richAvailable) { throw 'Rich text MCP API is missing' }
    if ($richAvailable) {
        $initialFormat = Tool $s1 'get_note_formatting' @{id=$noteId}
        Check ($initialFormat.unit -eq 'unicode_scalar' -and $initialFormat.runs.Count -eq 0) 'formatting model not sparse Unicode scalar range'
        Check ([int]$initialFormat.length -lt $expectedBody.Length) 'agent counts UTF16 code units instead of Unicode scalar values'
        $null=Tool $s1 'format_text_range' @{id=$noteId;start=1;end=7;unit='unicode_scalar';bold=$true;color='#BB1177';size=21}
        $null=Tool $s2 'format_text_range' @{id=$noteId;start=5;end=9;unit='unicode_scalar';font_family='Mono'}
        $formatted=Tool $s1 'get_note_formatting' @{id=$noteId}
        Check-Runs $formatted.runs ([int]$formatted.length)
        Check ((Style-At $formatted.runs 1).bold -and (Style-At $formatted.runs 5).font_family -eq 'Mono') 'overlapping rich formatting loses attributes'
        Check ((Style-At $formatted.runs 8).font_family -eq 'Mono' -and !(Style-At $formatted.runs 8).bold) 'rich style leaked beyond formatted selection'
        $unchanged = (ConvertTo-Json -InputObject @($formatted.runs) -Depth 15 -Compress)
        $null=Tool $s2 'format_text_range' @{id=$noteId;start=1;end=7;unit='unicode_scalar';bold=$true;color='#BB1177';size=21}
        $repeated = Tool $s1 'get_note_formatting' @{id=$noteId}
        Check ((ConvertTo-Json -InputObject @($repeated.runs) -Depth 15 -Compress) -ceq $unchanged) 'repeat apply duplicated or changed style spans'
        foreach($bad in @(
            @{start=5;end=5;unit='unicode_scalar';bold=$true},
            @{start=5;end=99999;unit='unicode_scalar';bold=$true},
            @{start=0;end=1;unit='utf16';bold=$true},
            @{start=0;end=1;unit='unicode_scalar';color='red'},
            @{start=0;end=1;unit='unicode_scalar';size=41},
            @{start=0;end=1;unit='unicode_scalar';italic=$true}
        )) {
            $bad.id=$noteId
            $null=Tool $s2 'format_text_range' $bad -ExpectError
        }
        $afterRejected=Tool $s1 'get_note_formatting' @{id=$noteId}
        Check ((ConvertTo-Json -InputObject @($afterRejected.runs) -Depth 15 -Compress) -ceq $unchanged) 'failed rich formatting partially mutated persisted runs'
        $pass++
        Write-Output 'PASS Unicode-scalar overlaps, attribute composition, repeated formatting idempotence and invalid patch atomicity'

        # e + combining accent occupies two scalar positions [5,7) but is
        # replaced by one NFC character. Subsequent spans must shift by one.
        $null=Tool $s2 'replace_text_range' @{id=$noteId;start=5;end=7;unit='unicode_scalar';text='é'}
        $expectedBody=$expectedBody.Replace(('e'+[char]0x0301), 'é')
        $afterAccent=Tool $s1 'read_note' @{id=$noteId}
        Check ($afterAccent.body -ceq $expectedBody) 'combining sequence replacement damaged Unicode text'
        $styleAfter=Tool $s1 'get_note_formatting' @{id=$noteId}
        Check-Runs $styleAfter.runs ([int]$styleAfter.length)
        Check ((Style-At $styleAfter.runs 5).font_family -eq 'Mono') 'formatted replacement did not inherit selected glyph style'
        $null=Tool $s1 'replace_text_range' @{id=$noteId;start=7;end=7;unit='unicode_scalar';text='🦊'}
        $expectedBody=$expectedBody.Replace('é 中','é 🦊中')
        $afterInsert=Tool $s2 'read_note' @{id=$noteId}
        Check ($afterInsert.body -ceq $expectedBody) 'emoji scalar insertion misaligned the text'
        Check-Runs $afterInsert.rich_text ([int](Tool $s1 'get_note_formatting' @{id=$noteId}).length)
        $null=Tool $s1 'clear_text_formatting' @{id=$noteId;start=1;end=5;unit='unicode_scalar'}
        $cleared=Tool $s2 'get_note_formatting' @{id=$noteId}
        Check (!(Style-At $cleared.runs 2).bold -and (Style-At $cleared.runs 5).bold) 'clear selection erased formatting outside its range'
        Check-Runs $cleared.runs ([int]$cleared.length)
        $pass++
        Write-Output 'PASS Unicode combining replacement, emoji insertion, span rebasing and selected-range reset'

        # Rich spans must survive unrelated picture metadata operations.
        $beforeMediaChange=Tool $s1 'get_note_formatting' @{id=$noteId}
        $richBeforeMedia=ConvertTo-Json -InputObject @($beforeMediaChange.runs) -Depth 20 -Compress
        $pictureId=[string]$embedded.image.id
        $null=Tool $s1 'image_resize' @{id=$noteId;image_id=$pictureId;width_percent=10}
        $null=Tool $s2 'image_caption' @{id=$noteId;image_id=$pictureId;caption='Mixed 🦊 <styled> picture'}
        $afterMediaChange=Tool $s2 'get_note_formatting' @{id=$noteId}
        Check ((ConvertTo-Json -InputObject @($afterMediaChange.runs) -Depth 20 -Compress) -ceq $richBeforeMedia) 'image resize or caption damaged rich text spans'
        $pass++
        Write-Output 'PASS image resize and Unicode caption preserve rich-text ranges byte-for-byte'

        # HTML is advertised as a portable complete-note export. Probe whether
        # it includes explicit text style attributes, distinct from global CSS.
        $styledExport=Join-Path $out 'rich-complete-note.html'
        $null=Tool $s1 'image_export_html' @{id=$noteId;path=$styledExport;user_approved=$true}
        $htmlContent=[System.IO.File]::ReadAllText($styledExport,[System.Text.Encoding]::UTF8)
        Check ($htmlContent.Contains('data:image/png;base64,') -and $htmlContent.Contains('&lt;tag&gt;')) 'styled-note HTML export damaged media or text escaping'
        $hasAllStyles=$htmlContent.Contains('#BB1177') -and
            $htmlContent.Contains('font-weight:700;') -and
            $htmlContent.Contains('font-size:21px;') -and
            $htmlContent.Contains('font-family:Consolas,monospace;')
        if (!$hasAllStyles) {
            Write-Output 'KNOWN GAP HTML export does not preserve all rich attributes (bold/color/size/family)'
            if ($RequireStyledHtml) { throw 'Complete-note HTML rich formatting not preserved' }
        } else {
            Write-Output 'PASS per-range bold, color, font-size and family survived escaped self-contained HTML export'
            $pass++
        }
    } else {
        Write-Output 'PENDING richer formatting API not yet present in the agent binary'
    }

    # Exact selected-text rich clipboard interchange via MCP, not OS clipboard.
    # Expected_text / expected_body guards deliberately reject stale editing
    # from another agent, while source font defaults survive across notes.
    $extendedNames=@('format_text_batch','copy_rich_fragment','paste_rich_fragment')
    $selectionApi=@($extendedNames | Where-Object { $_ -notin $toolNames }).Count -eq 0
    if ($RequireSelectionMcp -and !$selectionApi) { throw 'Missing batch-format/rich-fragment MCP tools' }
    if ($selectionApi) {
        $srcNote=Tool $s1 'create_note' @{title='Copy source 🦊';body=('A👩‍🚀e'+[char]0x0301+'猫Z')}
        $targetNote=Tool $s2 'create_note' @{title='Paste target';body='X日Y'}
        $srcId=[string]$srcNote.id
        $destId=[string]$targetNote.id
        $null=Tool $s1 'set_font_size' @{id=$srcId;size=26}
        $null=Tool $s2 'set_font_choice' @{id=$srcId;choice='Serif'}
        $null=Tool $s2 'set_font_choice' @{id=$destId;choice='Mono'}
        $batch=Tool $s1 'format_text_batch' @{id=$srcId;unit='unicode_scalar';expected_body=('A👩‍🚀e'+[char]0x0301+'猫Z');operations=@(
            @{start=1;end=4;bold=$true;color='#990033'},
            @{start=4;end=6;size=32}
        )}
        Check ($batch.applied -eq 2) 'two approved style patches were not applied atomically'
        $beforeInvalid=Tool $s2 'read_note' @{id=$srcId}
        foreach ($badOps in @(
            @(@{start=1;end=4;bold=$true},@{start=99;end=100;color='#ABCDEF'}),
            @(@{start=0;end=2;bold=$true}),
            @(@{start=1;end=4;color='red'})
        )) {
            $null=Tool $s1 'format_text_batch' @{id=$srcId;unit='unicode_scalar';operations=@($badOps)} -ExpectError
        }
        $null=Tool $s2 'format_text_batch' @{id=$srcId;unit='unicode_scalar';expected_body='stale text';operations=@(@{start=1;end=4;bold=$false})} -ExpectError
        $afterInvalid=Tool $s2 'read_note' @{id=$srcId}
        Check (($afterInvalid | ConvertTo-Json -Compress -Depth 30) -ceq ($beforeInvalid | ConvertTo-Json -Compress -Depth 30)) 'malformed/stale batch partially mutated a note'
        $pass++
        Write-Output 'PASS selected-range batch formatting is atomic, grapheme safe, stale-version guarded'

        $null=Tool $s2 'copy_rich_fragment' @{id=$srcId;unit='unicode_scalar';start=1;end=2} -ExpectError
        $null=Tool $s2 'copy_rich_fragment' @{id=$srcId;unit='unicode_scalar';start=4;end=5} -ExpectError
        $fragment=Tool $s1 'copy_rich_fragment' @{id=$srcId;unit='unicode_scalar';start=1;end=6}
        Check ($fragment.text -ceq ('👩‍🚀e'+[char]0x0301) -and $fragment.runs.Count -eq 2) 'portable fragment does not preserve styled Unicode'
        Check ($fragment.defaults.size -eq 26 -and $fragment.defaults.font_family -eq 'Serif') 'cross-note font defaults lost during copy'
        $originalTarget=Tool $s2 'read_note' @{id=$destId}
        $null=Tool $s2 'paste_rich_fragment' @{id=$destId;start=1;end=2;unit='unicode_scalar';expected_text='wrong';fragment=$fragment} -ExpectError
        $wrongFragment=@{format_version=1;unit='unicode_scalar';text='x';runs=@(@{start=0;end=5;style=@{bold=$true}})}
        $null=Tool $s2 'paste_rich_fragment' @{id=$destId;start=1;end=2;unit='unicode_scalar';expected_text='日';fragment=$wrongFragment} -ExpectError
        Check (((Tool $s2 'read_note' @{id=$destId}) | ConvertTo-Json -Compress -Depth 30) -ceq ($originalTarget | ConvertTo-Json -Compress -Depth 30)) 'invalid paste changed target note'
        $pasted=Tool $s2 'paste_rich_fragment' @{id=$destId;unit='unicode_scalar';start=1;end=2;expected_text='日';fragment=$fragment}
        Check ($pasted.inserted -eq 5) 'MCP paste counted UTF8 bytes instead of Unicode scalars'
        $dest=Tool $s1 'read_note' @{id=$destId}
        Check ($dest.body -ceq ('X👩‍🚀e'+[char]0x0301+'Y')) 'cross-note paste did not preserve source/destination body'
        Check ((Style-At $dest.rich_text 1).bold -and (Style-At $dest.rich_text 1).size -eq 26 -and (Style-At $dest.rich_text 1).font_family -eq 'Serif') 'cross-note paste did not preserve first span formatting and note defaults'
        Check ((Style-At $dest.rich_text 4).size -eq 32) 'cross-note paste dropped second span font size'
        Check-Runs $dest.rich_text ([int](Tool $s1 'get_note_formatting' @{id=$destId}).length)
        $pass++
        Write-Output 'PASS copy/paste Unicode grapheme protection, styled runs and source defaults, bad fragment atomicity'
    }

    # User settings must never be overwritten by MCP note operations. The
    # theme field is intentionally tested as a forward-compatible preference.
    $settingsPath = Join-Path $data 'ui-settings.json'
    $settings = '{ "sidebar_collapsed": true, "theme": "dark" }'
    [System.IO.File]::WriteAllText($settingsPath,$settings,(New-Object System.Text.UTF8Encoding($false)))
    $themeNames=@('get_app_settings','set_app_theme','set_sidebar_collapsed')
    $themeAvailable=@($themeNames | Where-Object { $_ -notin $toolNames }).Count -eq 0
    if ($RequireRich -and !$themeAvailable) { throw 'Appearance theme MCP API is missing' }
    if ($themeAvailable) {
        $setting=Tool $s1 'get_app_settings' @{}
        Check ($setting.theme -eq 'dark' -and $setting.sidebar_collapsed) 'stored theme/sidebar state unreadable'
        $aTheme=Send $s1 'tools/call' @{name='set_app_theme';arguments=@{theme='system'}}
        $bCollapsed=Send $s2 'tools/call' @{name='set_sidebar_collapsed';arguments=@{collapsed=$false}}
        Check (!(Receive $s1 $aTheme 'concurrent set theme').isError) 'theme update failed'
        Check (!(Receive $s2 $bCollapsed 'concurrent sidebar').isError) 'sidebar update failed'
        $both=Tool $s1 'get_app_settings' @{}
        Check ($both.theme -eq 'system' -and !$both.sidebar_collapsed) 'simultaneous theme/sidebar changes lost setting'
        $null=Tool $s2 'set_app_theme' @{theme='invalid'} -ExpectError
        $finalTheme=Tool $s1 'set_app_theme' @{theme='dark'}
        Check ($finalTheme.theme -eq 'dark' -and !$finalTheme.sidebar_collapsed) 'theme cycle/reset damaged sidebar selection'
        $pass++
        Write-Output 'PASS theme light/dark/system contract, concurrent settings updates, invalid theme rejection and sidebar preservation'
    }
    $settingsHash = (Get-FileHash -LiteralPath $settingsPath -Algorithm SHA256).Hash
    $null = Tool $s1 'append_to_note' @{id=$noteId;text='Agent metadata test'}
    Check ((Get-FileHash -LiteralPath $settingsPath -Algorithm SHA256).Hash -eq $settingsHash) 'MCP note write modified private theme/sidebar settings'
    $pass++
    Write-Output 'PASS MCP workspace writes do not overwrite theme/sidebar preferences'

    # Concurrent submissions deliberately overlap before reading their replies.
    foreach($i in 1..10) {
        $r1 = Send $s1 'tools/call' @{name='append_to_note';arguments=@{id=$noteId;text="agent_A_$i"}}
        $r2 = Send $s2 'tools/call' @{name='append_to_note';arguments=@{id=$noteId;text="agent_B_$i"}}
        $o1 = Receive $s1 $r1 'first concurrent append'
        $o2 = Receive $s2 $r2 'second concurrent append'
        Check (!$o1.isError -and !$o2.isError) 'concurrent append failed'
    }
    $concurrent = Tool $s1 'read_note' @{id=$noteId}
    foreach($i in 1..10) {
        Check ($concurrent.body.Contains("agent_A_$i") -and $concurrent.body.Contains("agent_B_$i")) "lost concurrent append $i"
    }
    Check ($concurrent.strokes.Count -eq 1 -and $concurrent.images.Count -eq 1) 'cross-process update removed drawings or pictures'
    if ($richAvailable) {
        # A range-format write may race with another agent appending text.
        # Neither operation may invalidate ranges or discard the other's work.
        for($i=1;$i -le 5;$i++) {
            $fa=Send $s1 'tools/call' @{name='format_text_range';arguments=@{
                id=$noteId;start=0;end=1;unit='unicode_scalar';bold=$true
            }}
            $tb=Send $s2 'tools/call' @{name='append_to_note';arguments=@{
                id=$noteId;text="styled_interleave_$i"
            }}
            Check (!(Receive $s1 $fa 'parallel format').isError) 'parallel format failed'
            Check (!(Receive $s2 $tb 'parallel append').isError) 'parallel append failed'
        }
        $interleaved=Tool $s2 'read_note' @{id=$noteId}
        foreach($i in 1..5) { Check ($interleaved.body.Contains("styled_interleave_$i")) 'interleaved format lost append' }
        $interleavedFormat=Tool $s2 'get_note_formatting' @{id=$noteId}
        Check-Runs $interleavedFormat.runs ([int]$interleavedFormat.length)
    }
    $pass++
    Write-Output 'PASS concurrent MCP text writes and format-vs-append transactions preserve runs and adjacent data'

    # Restart is a real process restart (still no desktop/UI).
    Close-Session $s1; $s1=$null
    Close-Session $s2; $s2=$null
    $s1=Session $AgentExe $data
    $saved=Tool $s1 'read_note' @{id=$noteId}
    Check ($saved.body.Contains($expectedBody) -and $saved.strokes.Count -eq 1 -and $saved.images.Count -eq 1 -and $saved.font_size -eq 24) 'restart lost text/drawing/picture/font preferences'
    if ($richAvailable) {
        $persistedFormat=Tool $s1 'get_note_formatting' @{id=$noteId}
        Check ($persistedFormat.runs.Count -gt 0) 'restart lost sparse style ranges'
        Check-Runs $persistedFormat.runs ([int]$persistedFormat.length)
    }
    $pass++
    Write-Output 'PASS MCP process restart retains mixed Unicode, drawings and font preferences'

    # Corrupt primary only in THIS disposable workspace. Confirm backup read
    # and zero silent overwrite, including settings isolation.
    $primaryPath=Join-Path $data 'workspace.json'
    $backupPath=Join-Path $data 'workspace.json.bak'
    Check (Test-Path -LiteralPath $backupPath) 'missing previous-save backup'
    $backup = [System.IO.File]::ReadAllBytes($backupPath)
    $backupDocument = [System.Text.Encoding]::UTF8.GetString($backup) | ConvertFrom-Json
    $expected = @($backupDocument.notes | Where-Object { $_.id -eq $noteId })
    Check ($expected.Count -eq 1) 'backup lost test note'
    [System.IO.File]::WriteAllText($primaryPath, '{"corrupt":"unfinished')
    $recovered=Tool $s1 'read_note' @{id=$noteId}
    Check ($recovered.body -ceq $expected[0].body) 'corrupt-primary fallback returned wrong backup text'
    Check ([System.IO.File]::ReadAllText($primaryPath) -eq '{"corrupt":"unfinished') 'agent silently overwrote corrupt primary'
    Check ([Convert]::ToBase64String([System.IO.File]::ReadAllBytes($backupPath)) -eq [Convert]::ToBase64String($backup)) 'backup changed during read-only recovery'
    Check ((Get-FileHash -LiteralPath $settingsPath -Algorithm SHA256).Hash -eq $settingsHash) 'corrupt workspace affected UI settings'
    $pass++
    Write-Output 'PASS corrupt primary recovers previous snapshot without overwriting evidence or theme settings'

    if ($richAvailable) {
        # A valid JSON document can still carry an invalid rich-text interval.
        # Treat it as corruption and recover the last good backup atomically.
        $malformed = [System.Text.Encoding]::UTF8.GetString($backup) | ConvertFrom-Json
        $malformedNote = @($malformed.notes | Where-Object { $_.id -eq $noteId })[0]
        Check ($malformedNote.rich_text.Count -gt 0) 'recovery backup unexpectedly lacks formatted spans'
        $malformedNote.rich_text[0].start=999999
        $malformedNote.rich_text[0].end=1000000
        $invalidEncoded = ConvertTo-Json -InputObject $malformed -Depth 64 -Compress
        [System.IO.File]::WriteAllText($primaryPath,$invalidEncoded,(New-Object System.Text.UTF8Encoding($false)))
        $invalidDigest = (Get-FileHash -LiteralPath $primaryPath -Algorithm SHA256).Hash
        $backupNote=Tool $s1 'read_note' @{id=$noteId}
        Check ($backupNote.body -ceq $expected[0].body) 'invalid rich-run JSON displaced valid backup'
        Check ((Get-FileHash -LiteralPath $primaryPath -Algorithm SHA256).Hash -eq $invalidDigest) 'agent overwrote invalid rich primary'
        Check ([Convert]::ToBase64String([System.IO.File]::ReadAllBytes($backupPath)) -eq [Convert]::ToBase64String($backup)) 'invalid rich-range recovery overwrote good backup'
        $pass++
        Write-Output 'PASS structurally invalid rich span falls back to valid backup without discarding evidence'
    }

    # A damaged theme primary should use its valid previous-save backup. With
    # BOTH copies corrupt, mutations must reject rather than fabricate state.
    if ($themeAvailable) {
        $themeBytes=[System.IO.File]::ReadAllBytes($settingsPath)
        $settingsBackup=Join-Path $data 'ui-settings.json.bak'
        Check (Test-Path -LiteralPath $settingsBackup -PathType Leaf) 'theme settings backup missing'
        $settingsBackupBytes=[System.IO.File]::ReadAllBytes($settingsBackup)
        $settingsRecovered=[System.Text.Encoding]::UTF8.GetString($settingsBackupBytes) | ConvertFrom-Json
        [System.IO.File]::WriteAllText($settingsPath,'{broken settings')
        $badHash=(Get-FileHash -LiteralPath $settingsPath -Algorithm SHA256).Hash
        $themeFallback=Tool $s1 'get_app_settings' @{}
        Check ($themeFallback.theme -eq $settingsRecovered.theme -and
            $themeFallback.sidebar_collapsed -eq $settingsRecovered.sidebar_collapsed) 'corrupt settings primary did not recover valid backup'
        Check ((Get-FileHash -LiteralPath $settingsPath -Algorithm SHA256).Hash -eq $badHash) 'theme recovery overwrote damaged original'
        [System.IO.File]::WriteAllText($settingsBackup,'{broken backup')
        $null=Tool $s1 'set_app_theme' @{theme='light'} -ExpectError
        Check ((Get-FileHash -LiteralPath $settingsPath -Algorithm SHA256).Hash -eq $badHash) 'failed theme read modified malformed settings'
        [System.IO.File]::WriteAllBytes($settingsPath,$themeBytes)
        [System.IO.File]::WriteAllBytes($settingsBackup,$settingsBackupBytes)
        $pass++
        Write-Output 'PASS corrupt theme primary recovers valid backup; double-corrupt settings reject mutation without loss'
    }

    # Migration: load a minimal old workspace record without rich_text, then
    # write through today's MCP while keeping old text and board schema.
    $legacyDir=Join-Path $run 'legacy-workspace'
    New-Item -ItemType Directory -Force -Path $legacyDir | Out-Null
    $legacyBody='Legacy café 🦊 中 & <tag>'
    $legacyDoc=@{
        version=1;active_id='legacy-abc';notes=@(@{
            id='legacy-abc';title='Old UTF8 note';body=$legacyBody;kind='note';
            font_size=17;font_choice='Sans';pinned=$false;strokes=@();images=@();updated=1
        })
    }
    [System.IO.File]::WriteAllText((Join-Path $legacyDir 'workspace.json'),
        (ConvertTo-Json -InputObject $legacyDoc -Depth 20),(New-Object System.Text.UTF8Encoding($false)))
    $legacySession=$null
    try {
        $legacySession=Session $AgentExe $legacyDir
        $old=Tool $legacySession 'read_note' @{id='legacy-abc'}
        Check ($old.body -ceq $legacyBody) 'legacy note body migration lost Unicode'
        if ($richAvailable) {
            Check (($null -eq $old.PSObject.Properties['rich_text']) -or ($old.rich_text.Count -eq 0)) 'legacy file acquired fabricated formatting'
            $null=Tool $legacySession 'format_text_range' @{id='legacy-abc';start=0;end=6;unit='unicode_scalar';bold=$true}
            $old=Tool $legacySession 'read_note' @{id='legacy-abc'}
            Check ($old.rich_text.Count -gt 0 -and $old.body -ceq $legacyBody) 'legacy note formatting upgrade lost text'
        }
    } finally { Close-Session $legacySession }
    $pass++
    Write-Output 'PASS legacy workspace with no rich field loads and upgrades without losing Unicode body'

    # Rich formatting is a separate contract. Legacy note-level font choices
    # alone are not proof of formatting inside a single paragraph.
    $rich = @(Text-RichFields $recovered)
    $richTools = @($toolNames | Where-Object { $_ -in $richNames })
    if ($rich.Count -eq 0 -and $richTools.Count -eq 0) {
        $message='PENDING: no rich span/document contract in current agent or persisted note model'
        if ($RequireRich) { throw $message }
        Write-Output $message
    } else {
        Write-Output ("RICH CONTRACT DETECTED fields=[{0}] tools=[{1}]" -f ($rich -join ','),($richTools -join ','))
        if ($RequireRich) { Check ($richAvailable) 'document exposed rich format without usable MCP tools' }
    }
} finally {
    Close-Session $s2
    Close-Session $s1
    Write-Output "HEADLESS_PASSED=$pass"
    Write-Output "QA_ISOLATED_WORKSPACE=$run"
}
