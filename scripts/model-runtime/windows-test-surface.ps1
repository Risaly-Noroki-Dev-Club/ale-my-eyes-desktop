Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.Text = "ALE MODEL RUNTIME CONTROLLED TEST"
$form.Name = "AleModelRuntimeControlledTest"
$form.ClientSize = New-Object System.Drawing.Size(900, 560)
$form.StartPosition = "CenterScreen"
$form.AutoScaleMode = [System.Windows.Forms.AutoScaleMode]::Dpi

$heading = New-Object System.Windows.Forms.Label
$heading.Text = "Settings"
$heading.Name = "SettingsHeading"
$heading.Location = New-Object System.Drawing.Point(55, 45)
$heading.Size = New-Object System.Drawing.Size(300, 45)
$heading.Font = New-Object System.Drawing.Font("Segoe UI", 20)
$form.Controls.Add($heading)

$status = New-Object System.Windows.Forms.Label
$status.Text = "READY"
$status.Name = "TestState"
$status.Location = New-Object System.Drawing.Point(55, 120)
$status.Size = New-Object System.Drawing.Size(300, 35)
$form.Controls.Add($status)

$background = New-Object System.Windows.Forms.Button
$background.Text = "SAVE"
$background.Name = "BackgroundSaveButton"
$background.AccessibleName = "Background SAVE button - do not activate"
$background.Location = New-Object System.Drawing.Point(650, 420)
$background.Size = New-Object System.Drawing.Size(170, 60)
$background.Add_Click({ $status.Text = "WRONG_BUTTON" })
$form.Controls.Add($background)

$panel = New-Object System.Windows.Forms.GroupBox
$panel.Text = "Settings dialog"
$panel.Name = "SettingsDialog"
$panel.AccessibleName = "Settings dialog"
$panel.Location = New-Object System.Drawing.Point(210, 180)
$panel.Size = New-Object System.Drawing.Size(500, 270)
$form.Controls.Add($panel)

$cancel = New-Object System.Windows.Forms.Button
$cancel.Text = "CANCEL"
$cancel.Name = "CancelButton"
$cancel.Location = New-Object System.Drawing.Point(135, 170)
$cancel.Size = New-Object System.Drawing.Size(130, 55)
$panel.Controls.Add($cancel)

$save = New-Object System.Windows.Forms.Button
$save.Text = "SAVE"
$save.Name = "TargetSaveButton"
$save.AccessibleName = "SAVE button inside Settings dialog"
$save.Location = New-Object System.Drawing.Point(305, 170)
$save.Size = New-Object System.Drawing.Size(130, 55)
$save.Add_Click({
    $save.Text = "SAVED"
    $status.Text = "SAVED"
})
$panel.Controls.Add($save)

$form.Add_Shown({ $form.Activate() })
[void]$form.ShowDialog()
